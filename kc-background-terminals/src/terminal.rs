use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const OUTPUT_CAP: usize = 256 * 1024;
const READ_CAP: usize = 64 * 1024;
const INPUT_CAP: usize = 64 * 1024;
const TERMINAL_CAP: usize = 128;
const POLL: Duration = Duration::from_millis(20);
struct State {
    output: VecDeque<u8>,
    end: u64,
    status: &'static str,
    exit_code: Option<i32>,
    signal: Option<i32>,
}
struct Terminal {
    id: String,
    command: String,
    cwd: String,
    pid: u32,
    state: Arc<Mutex<State>>,
    stdin: Mutex<Option<ChildStdin>>,
    stop: Arc<AtomicBool>,
    watcher: Mutex<Option<JoinHandle<()>>>,
}
pub struct TerminalManager {
    terminals: Mutex<HashMap<String, Arc<Terminal>>>,
    next_id: Mutex<u64>,
    shutting_down: AtomicBool,
}

fn nonblocking(fd: i32) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
fn kill_group(pid: u32) {
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}
fn drain<R: Read + Send + 'static>(
    mut pipe: R,
    state: Arc<Mutex<State>>,
    finished: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let mut s = state.lock().unwrap();
                    s.end += n as u64;
                    s.output.extend(&buf[..n]);
                    let excess = s.output.len().saturating_sub(OUTPUT_CAP);
                    s.output.drain(..excess);
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if finished.load(Ordering::Acquire) {
                        break;
                    }
                    thread::sleep(POLL);
                }
                Err(_) => break,
            }
        }
    })
}
impl Terminal {
    fn info(&self) -> Value {
        let s = self.state.lock().unwrap();
        json!({"id":self.id,"command":self.command,"cwd":self.cwd,"pid":self.pid,"status":s.status,"exit_code":s.exit_code,"signal":s.signal,"output_start":s.end-s.output.len() as u64,"output_end":s.end})
    }
}
impl TerminalManager {
    pub fn new() -> Self {
        Self {
            terminals: Mutex::new(HashMap::new()),
            next_id: Mutex::new(1),
            shutting_down: AtomicBool::new(false),
        }
    }
    pub fn start(
        &self,
        command: &str,
        cwd: &Path,
        timeout_ms: Option<u64>,
    ) -> Result<Value, String> {
        if command.trim().is_empty() {
            return Err("command must not be empty".into());
        }
        if timeout_ms == Some(0) {
            return Err("timeout_ms must be greater than zero".into());
        }
        let mut terminals = self.terminals.lock().unwrap();
        if self.shutting_down.load(Ordering::Acquire) {
            return Err("terminal manager is shutting down".into());
        }
        if terminals.len() >= TERMINAL_CAP {
            let finished = terminals
                .values()
                .filter(|t| t.state.lock().unwrap().status != "running")
                .min_by_key(|t| {
                    t.id.strip_prefix("term-")
                        .and_then(|n| n.parse::<u64>().ok())
                        .unwrap_or(u64::MAX)
                })
                .map(|t| t.id.clone());
            if let Some(id) = finished {
                terminals.remove(&id);
            } else {
                return Err(format!(
                    "terminal limit reached ({TERMINAL_CAP} running terminals)"
                ));
            }
        }
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg(command)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .map_err(|e| format!("could not start command: {e}"))?;
        let pid = child.id();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        if let Err(e) = nonblocking(stdin.as_raw_fd())
            .and_then(|_| nonblocking(stdout.as_raw_fd()))
            .and_then(|_| nonblocking(stderr.as_raw_fd()))
        {
            kill_group(pid);
            let _ = child.wait();
            return Err(format!("could not configure pipes: {e}"));
        }
        let mut next = self.next_id.lock().unwrap();
        let id = format!("term-{}", *next);
        *next += 1;
        let state = Arc::new(Mutex::new(State {
            output: VecDeque::new(),
            end: 0,
            status: "running",
            exit_code: None,
            signal: None,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let terminal = Arc::new(Terminal {
            id: id.clone(),
            command: command.into(),
            cwd: cwd.display().to_string(),
            pid,
            state: state.clone(),
            stdin: Mutex::new(Some(stdin)),
            stop: stop.clone(),
            watcher: Mutex::new(None),
        });
        let finished = Arc::new(AtomicBool::new(false));
        let out = drain(stdout, state.clone(), finished.clone());
        let err = drain(stderr, state.clone(), finished.clone());
        let watcher = thread::spawn(move || {
            let started = Instant::now();
            loop {
                let stopped = stop.load(Ordering::Acquire);
                let timed_out = timeout_ms
                    .map(|ms| started.elapsed() >= Duration::from_millis(ms))
                    .unwrap_or(false);
                if stopped || timed_out {
                    kill_group(pid);
                }
                match child.try_wait() {
                    Ok(Some(exit)) => {
                        kill_group(pid);
                        finished.store(true, Ordering::Release);
                        let _ = out.join();
                        let _ = err.join();
                        let mut s = state.lock().unwrap();
                        s.status = if stopped {
                            "stopped"
                        } else if timed_out {
                            "timed_out"
                        } else {
                            "exited"
                        };
                        s.exit_code = exit.code();
                        s.signal = exit.signal();
                        break;
                    }
                    Ok(None) => thread::sleep(POLL),
                    Err(_) => {
                        kill_group(pid);
                        let _ = child.wait();
                        finished.store(true, Ordering::Release);
                        let _ = out.join();
                        let _ = err.join();
                        state.lock().unwrap().status = "error";
                        break;
                    }
                }
            }
        });
        *terminal.watcher.lock().unwrap() = Some(watcher);
        let info = terminal.info();
        terminals.insert(id, terminal);
        Ok(info)
    }
    fn get(&self, id: &str) -> Result<Arc<Terminal>, String> {
        self.terminals
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| format!("unknown terminal: {id}"))
    }
    pub fn list(&self) -> Value {
        let terminals = self.terminals.lock().unwrap();
        let mut values: Vec<_> = terminals.values().map(|t| t.info()).collect();
        values.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        json!({"terminals":values})
    }
    pub fn read(&self, id: &str, cursor: Option<u64>) -> Result<Value, String> {
        let terminal = self.get(id)?;
        let s = terminal.state.lock().unwrap();
        let start = s.end - s.output.len() as u64;
        let requested = cursor.unwrap_or(start);
        if requested > s.end {
            return Err(format!("cursor {requested} is beyond output end {}", s.end));
        }
        let actual = requested.max(start);
        let data: Vec<u8> = s
            .output
            .iter()
            .skip((actual - start) as usize)
            .take(READ_CAP)
            .copied()
            .collect();
        Ok(
            json!({"id":id,"output":String::from_utf8_lossy(&data),"cursor":actual+data.len() as u64,"output_start":start,"output_end":s.end,"truncated":requested<start,"status":s.status,"exit_code":s.exit_code,"signal":s.signal}),
        )
    }
    pub fn write(&self, id: &str, input: &str) -> Result<Value, String> {
        if input.len() > INPUT_CAP {
            return Err(format!("input exceeds {INPUT_CAP} bytes"));
        }
        let terminal = self.get(id)?;
        if terminal.state.lock().unwrap().status != "running" {
            return Err("terminal is not running".into());
        }
        let mut stdin = terminal.stdin.lock().unwrap();
        let stdin = stdin.as_mut().ok_or("terminal stdin is closed")?;
        match stdin.write(input.as_bytes()) {
            Ok(written) => Ok(json!({"id":id,"written":written})),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                Ok(json!({"id":id,"written":0}))
            }
            Err(e) => Err(format!("could not write input: {e}")),
        }
    }
    pub fn stop(&self, id: &str) -> Result<Value, String> {
        let terminal = self.get(id)?;
        terminal.stop.store(true, Ordering::Release);
        if let Some(watcher) = terminal.watcher.lock().unwrap().take() {
            let _ = watcher.join();
        }
        terminal.stdin.lock().unwrap().take();
        Ok(terminal.info())
    }
    pub fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::Release);
        let terminals = self.terminals.lock().unwrap();
        for t in terminals.values() {
            t.stop.store(true, Ordering::Release);
        }
        for t in terminals.values() {
            if let Some(w) = t.watcher.lock().unwrap().take() {
                let _ = w.join();
            }
            t.stdin.lock().unwrap().take();
        }
    }
}
impl Drop for TerminalManager {
    fn drop(&mut self) {
        self.shutdown();
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn wait(m: &TerminalManager, id: &str) -> Value {
        for _ in 0..300 {
            let v = m.read(id, None).unwrap();
            if v["status"] != "running" {
                return v;
            }
            thread::sleep(POLL);
        }
        panic!("terminal did not finish");
    }
    #[test]
    fn output_exit_and_cursor() {
        let m = TerminalManager::new();
        let v = m
            .start(
                "printf out; printf err >&2; exit 7",
                Path::new("/tmp"),
                None,
            )
            .unwrap();
        let id = v["id"].as_str().unwrap();
        let out = wait(&m, id);
        let text = out["output"].as_str().unwrap();
        assert!(text.contains("out") && text.contains("err"));
        assert_eq!(out["exit_code"], 7);
        assert_eq!(
            m.read(id, Some(out["cursor"].as_u64().unwrap())).unwrap()["output"],
            ""
        );
        assert!(m.read(id, Some(100000)).is_err());
    }
    #[test]
    fn input_and_stop() {
        let m = TerminalManager::new();
        let v = m
            .start(
                "read line; printf 'got:%s' \"$line\"",
                Path::new("/tmp"),
                None,
            )
            .unwrap();
        let id = v["id"].as_str().unwrap();
        assert_eq!(m.write(id, "hello\n").unwrap()["written"], 6);
        assert_eq!(wait(&m, id)["output"], "got:hello");
        let v = m.start("sleep 60", Path::new("/tmp"), None).unwrap();
        assert_eq!(
            m.stop(v["id"].as_str().unwrap()).unwrap()["status"],
            "stopped"
        );
    }
    #[test]
    fn dropping_manager_reaps_running_process() {
        let pid = {
            let m = TerminalManager::new();
            let v = m.start("exec sleep 60", Path::new("/tmp"), None).unwrap();
            v["pid"].as_i64().unwrap() as i32
        };
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
    #[test]
    fn finished_records_are_evicted_at_capacity() {
        let m = TerminalManager::new();
        let mut first = String::new();
        for i in 0..=TERMINAL_CAP {
            let value = m.start("true", Path::new("/tmp"), None).unwrap();
            let id = value["id"].as_str().unwrap();
            if i == 0 {
                first = id.to_owned();
            }
            wait(&m, id);
        }
        assert!(m.read(&first, None).is_err());
        assert_eq!(
            m.list()["terminals"].as_array().unwrap().len(),
            TERMINAL_CAP
        );
        m.shutdown();
        assert!(m.start("true", Path::new("/tmp"), None).is_err());
    }
    #[test]
    fn timeout_and_bounded_output() {
        let m = TerminalManager::new();
        let v = m.start("sleep 60", Path::new("/tmp"), Some(50)).unwrap();
        assert_eq!(wait(&m, v["id"].as_str().unwrap())["status"], "timed_out");
        let v = m
            .start("head -c 400000 /dev/zero", Path::new("/tmp"), None)
            .unwrap();
        let id = v["id"].as_str().unwrap();
        let out = wait(&m, id);
        assert_eq!(out["output_end"], 400000);
        assert!(m.read(id, Some(0)).unwrap()["truncated"].as_bool().unwrap());
        assert_eq!(
            out["cursor"].as_u64().unwrap() - out["output_start"].as_u64().unwrap(),
            READ_CAP as u64
        );
    }
}
