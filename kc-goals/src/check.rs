//! Running a criterion's check command.
//!
//! This is the only place the plugin executes anything, and it is deliberately
//! small: `/bin/sh -c <command>` in the workspace, a hard timeout, a bounded
//! amount of output, and an exit code. No shell built-ins are added, nothing is
//! interpolated, and the command is exactly the string a human or the agent wrote
//! into the goal file.
//!
//! **It is not a sandbox.** The command runs with the plugin's own OS rights,
//! which are the server user's rights. Verification is opt-in for that reason:
//! `settings.verify_commands = true` plus the `shell` permission, and every call
//! still goes through the host's approval policy like any other plugin tool.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Most output kept from either stream. A check that prints a whole test log is
/// still useful; the evidence field is capped again before it is stored.
pub const MAX_OUTPUT_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckOutcome {
    /// Exit code 0 and nothing else counts as passing.
    pub ok: bool,
    pub code: Option<i32>,
    /// Combined stdout and stderr, trimmed and capped.
    pub output: String,
    pub timed_out: bool,
    pub ms: u64,
}

impl CheckOutcome {
    /// One line for the tool result and the evidence field.
    pub fn summary(&self) -> String {
        if self.timed_out {
            return format!("timed out after {} ms", self.ms);
        }
        match self.code {
            Some(0) => format!("exit 0 in {} ms", self.ms),
            Some(code) => format!("exit {code} in {} ms", self.ms),
            None => format!("killed by a signal in {} ms", self.ms),
        }
    }
}

/// Run one command in `workspace`.
pub fn run(workspace: &str, command: &str, timeout: Duration) -> Result<CheckOutcome, String> {
    let command = command.trim();
    if command.is_empty() {
        return Err("the check command is empty".into());
    }
    let dir = Path::new(workspace);
    if !dir.is_dir() {
        return Err(format!("`{workspace}` is not a directory to run in"));
    }
    let started = Instant::now();
    let mut child = {
        let mut builder = Command::new("/bin/sh");
        builder
            .arg("-c")
            .arg(command)
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Own process group, so killing a check also kills what it started:
            // `sh -c "a | b"` and `sh -c "x; sleep 9"` both fork, and a SIGKILL
            // to the shell alone leaves the children holding the pipes.
            .process_group(0);
        builder
            .spawn()
            .map_err(|e| format!("could not start `/bin/sh -c {command}`: {e}"))?
    };
    let pgid = child.id();

    // Two reader threads: a command that fills a pipe while we wait on its exit
    // would otherwise deadlock the check.
    let (tx, rx) = mpsc::channel::<(bool, Vec<u8>)>();
    for (is_stderr, pipe) in [
        (false, child.stdout.take().map(Pipe::Out)),
        (true, child.stderr.take().map(Pipe::Err)),
    ] {
        let Some(pipe) = pipe else { continue };
        let tx = tx.clone();
        std::thread::spawn(move || {
            // Drain the whole pipe but keep only the head of it. Stopping the read
            // at the cap would let a chatty command fill the pipe and block on
            // write forever, which looks exactly like a hanging check.
            let mut handle: Box<dyn Read + Send> = match pipe {
                Pipe::Out(handle) => Box::new(handle),
                Pipe::Err(handle) => Box::new(handle),
            };
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                match handle.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        let room = MAX_OUTPUT_BYTES.saturating_sub(buffer.len());
                        if room > 0 {
                            buffer.extend_from_slice(&chunk[..n.min(room)]);
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = tx.send((is_stderr, buffer));
        });
    }
    drop(tx);

    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(error) => return Err(format!("could not wait for the check: {error}")),
        }
        if started.elapsed() >= timeout {
            // `kill -KILL -<pgid>`: the minimised version of what a terminal does
            // when you interrupt a command. Without it a grandchild survives and
            // keeps the read threads alive, which would hang this call instead of
            // the check.
            let _ = Command::new("kill")
                .arg("-KILL")
                .arg(format!("-{pgid}"))
                .status();
            let _ = child.kill();
            let _ = child.wait();
            timed_out = true;
            break None;
        }
        std::thread::sleep(Duration::from_millis(40));
    };

    let mut stdout = String::new();
    let mut stderr = String::new();
    // Both readers, with a deadline: after a kill the pipes *should* close at
    // once, and if they do not, a check must still return.
    let deadline = Instant::now() + Duration::from_secs(2);
    for _ in 0..2 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Ok((is_stderr, bytes)) = rx.recv_timeout(remaining) else {
            break;
        };
        let text = String::from_utf8_lossy(&bytes).to_string();
        if is_stderr {
            stderr.push_str(&text);
        } else {
            stdout.push_str(&text);
        }
    }
    let output = match (stdout.trim().is_empty(), stderr.trim().is_empty()) {
        (true, true) => String::new(),
        (false, true) => stdout.trim().to_string(),
        (true, false) => stderr.trim().to_string(),
        (false, false) => format!("{}\n{}", stdout.trim(), stderr.trim()),
    };
    let code = status.as_ref().and_then(std::process::ExitStatus::code);
    Ok(CheckOutcome {
        ok: !timed_out && code == Some(0),
        code,
        output,
        timed_out,
        ms: started.elapsed().as_millis() as u64,
    })
}

/// The two pipes, so the reader loop can be one closure each.
enum Pipe {
    Out(std::process::ChildStdout),
    Err(std::process::ChildStderr),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> std::path::PathBuf {
        crate::model::temp_dir("kc-goals-check")
    }

    fn timeout() -> Duration {
        Duration::from_secs(10)
    }

    #[test]
    fn a_zero_exit_is_a_pass_and_its_output_is_kept() {
        let ws = workspace();
        let outcome = run(ws.to_str().unwrap(), "echo verified", timeout()).unwrap();
        assert!(outcome.ok, "{outcome:?}");
        assert_eq!(outcome.code, Some(0));
        assert!(outcome.output.contains("verified"), "{outcome:?}");
        let _ = std::fs::remove_dir_all(ws);
    }

    #[test]
    fn a_non_zero_exit_fails_and_keeps_stderr() {
        let ws = workspace();
        let outcome = run(
            ws.to_str().unwrap(),
            "echo 'no such file' >&2; exit 3",
            timeout(),
        )
        .unwrap();
        assert!(!outcome.ok);
        assert_eq!(outcome.code, Some(3));
        assert!(outcome.output.contains("no such file"), "{outcome:?}");
        let _ = std::fs::remove_dir_all(ws);
    }

    #[test]
    fn the_command_runs_in_the_workspace() {
        let ws = workspace();
        std::fs::write(ws.join("marker.txt"), "here").unwrap();
        let outcome = run(ws.to_str().unwrap(), "cat marker.txt", timeout()).unwrap();
        assert!(outcome.ok);
        assert_eq!(outcome.output, "here");
        let _ = std::fs::remove_dir_all(ws);
    }

    #[test]
    fn the_workspace_and_environment_are_not_leaked_into_the_command() {
        let ws = workspace();
        let outcome = run(
            ws.to_str().unwrap(),
            "printf '%s' \"$KARUCODE_AUTH_TOKEN\"",
            timeout(),
        )
        .unwrap();
        // Not a claim about the environment in general: this is the variable the
        // plugin must never see, and it must not appear because a check ran.
        assert!(!outcome.output.contains("sk-"));
        let _ = std::fs::remove_dir_all(ws);
    }

    #[test]
    fn a_hanging_command_is_killed_at_the_deadline() {
        let ws = workspace();
        let started = std::time::Instant::now();
        let outcome = run(
            ws.to_str().unwrap(),
            "sleep 30",
            Duration::from_millis(400),
        )
        .unwrap();
        assert!(outcome.timed_out, "{outcome:?}");
        assert!(!outcome.ok);
        assert!(started.elapsed() < Duration::from_secs(10), "it really waited");
        let _ = std::fs::remove_dir_all(ws);
    }

    #[test]
    fn an_empty_command_is_refused() {
        let ws = workspace();
        assert!(run(ws.to_str().unwrap(), "   ", timeout()).is_err());
        let _ = std::fs::remove_dir_all(ws);
    }

    #[test]
    fn a_missing_workspace_is_refused() {
        let ws = workspace();
        let gone = ws.join("nope");
        assert!(run(gone.to_str().unwrap(), "true", timeout()).is_err());
        let _ = std::fs::remove_dir_all(ws);
    }

    #[test]
    fn a_check_that_writes_to_both_streams_does_not_deadlock() {
        let ws = workspace();
        // Enough output on stdout to fill a pipe buffer, then stderr.
        let outcome = run(
            ws.to_str().unwrap(),
            "for i in $(seq 1 4000); do echo line-$i; done; echo 'tail' >&2",
            timeout(),
        )
        .unwrap();
        assert!(outcome.ok, "{outcome:?}");
        assert!(outcome.output.contains("tail"), "stderr was lost");
        let _ = std::fs::remove_dir_all(ws);
    }
}
