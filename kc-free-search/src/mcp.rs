//! A minimal MCP stdio client, and the supervision of the `free-search-mcp`
//! backend process.
//!
//! # Why this exists
//!
//! karucode can already connect an MCP server (`[[mcp_servers]]`), so the honest
//! question is what a plugin adds. Four things, none of which the plain MCP path
//! has:
//!
//! 1. **Restart after death.** A failed MCP server in karucode stays failed
//!    forever — there is no health-check loop and no reconnect. Here a dead
//!    backend is respawned on the next call, and the budget resets after a
//!    successful call rather than counting against the plugin's whole lifetime.
//! 2. **A timeout that names the remedy.** karucode cuts a plugin call at 60 s.
//!    The backend is killed at `call_timeout_secs` first, so the model is told
//!    the search was abandoned and how to narrow it instead of receiving a
//!    generic harness timeout.
//! 3. **An agent-facing surface.** The upstream descriptions run to 7 kB each
//!    and are written for a chat client; see `tools.rs`.
//! 4. **A panel and a prompt hook.** See `main.rs`.
//!
//! # Shape
//!
//! One process, one request in flight at a time. A reader thread owns the
//! child's stdout and pushes parsed frames into a channel, so a call can wait on
//! a deadline instead of blocking forever in `read`. That is the whole
//! concurrency model: the plugin's own request loop (in the SDK) is
//! synchronous, so a second in-flight request is impossible by construction.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

/// The MCP revision this client speaks. The backend also serves older revisions;
/// this one is what `2025-06-18`-era clients send, and it is what makes
/// `resource_link` blocks legal in a result.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Cap on one protocol line. The host caps plugin lines at 8 MiB; a `fetch` that
/// inlines a 300 kB PNG arrives as base64 well under this, and anything larger
/// is a runaway result rather than something worth buffering.
pub const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

/// Consecutive backend failures tolerated before the plugin stops retrying.
///
/// Counted *consecutively*, and reset by any successful call: a long session that
/// hits one transient crash keeps working, while a backend that is broken for
/// real stops costing 30 s per tool call.
pub const MAX_CONSECUTIVE_FAILURES: u32 = 3;

/// What one backend process is configured to be.
#[derive(Debug, Clone)]
pub struct BackendConfig {
    pub command: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub start_timeout: Duration,
    pub call_timeout: Duration,
}

impl BackendConfig {
    pub fn command_line(&self) -> String {
        self.command.join(" ")
    }
}

/// A backend failure, phrased so the model knows what to do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpError {
    /// The process could not be started at all.
    Spawn(String),
    /// It started but never completed the MCP handshake.
    Handshake(String),
    /// It did not answer within the budget. The process was killed.
    Timeout { what: String, budget: Duration },
    /// It died, or broke the framing contract.
    Transport(String),
    /// It answered with a JSON-RPC error.
    Remote { code: i64, message: String },
    /// It answered with something that is not the MCP shape.
    Protocol(String),
}

impl std::fmt::Display for McpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(m) => write!(
                f,
                "could not start the search backend ({m}). Check `command` in the plugin \
                 settings, and that `uvx` is on PATH; on an air-gapped host pre-install the \
                 package and point `command` at the entry point."
            ),
            Self::Handshake(m) => write!(
                f,
                "the search backend started but did not complete the MCP handshake ({m}). \
                 Run the configured command by hand to see its error; the next call tries \
                 again."
            ),
            Self::Timeout { what, budget } => write!(
                f,
                "the search backend did not finish `{what}` within {} and was stopped. \
                 Narrow the request (fewer results, a lower `depth`, one URL instead of many) \
                 and call again; the process restarts on the next call.",
                human_duration(*budget)
            ),
            Self::Transport(m) => write!(
                f,
                "the search backend stopped unexpectedly ({m}). It restarts on the next call; \
                 if this repeats, run the configured command by hand to see its error."
            ),
            Self::Remote { code, message } => write!(f, "{message} (backend error {code})"),
            Self::Protocol(m) => write!(
                f,
                "the search backend returned an unexpected response ({m}). The call was not \
                 retried; the next call starts a fresh process."
            ),
        }
    }
}

impl std::error::Error for McpError {}

/// A budget as a human reads it: `55s`, `1.5s`, `400ms`.
///
/// `Duration::as_secs` truncates, so a sub-second budget would print "0s" and
/// make the message look like a bug in the plugin rather than a deadline.
fn human_duration(d: Duration) -> String {
    if d.as_secs() >= 1 {
        if d.subsec_millis() == 0 {
            format!("{}s", d.as_secs())
        } else {
            format!("{:.1}s", d.as_secs_f64())
        }
    } else {
        format!("{}ms", d.as_millis())
    }
}

/// Whether a failure is worth restarting the process for.
///
/// A `Remote` error is the tool refusing the arguments — the backend is healthy.
/// Everything else means the process is gone or wrong.
impl McpError {
    pub fn is_backend_failure(&self) -> bool {
        !matches!(self, Self::Remote { .. })
    }
}

/// One tool the backend advertises.
#[derive(Debug, Clone)]
pub struct BackendTool {
    pub name: String,
    /// Kept so a drift report can say how much the upstream text grew; the plugin
    /// never forwards it, since the curated descriptions in `tools.rs` are what
    /// the agent reads.
    pub description_len: usize,
    pub schema: Value,
}

/// What the backend said about itself during the handshake.
#[derive(Debug, Clone, Default)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
    /// The upstream `instructions` block.
    ///
    /// Deliberately **not** forwarded into the prompt: this plugin's
    /// `before_prompt` states the same discipline in less space and without the
    /// upstream's chat-client framing. It is kept so the panel can show what the
    /// backend asked for, which is what an operator needs when deciding whether a
    /// backend upgrade changed the guidance.
    pub instructions: String,
}

/// One frame the reader thread saw.
enum Inbound {
    Frame(Value),
    Eof(String),
}

/// Live counters, for the panel and for failure handling.
#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub calls: u64,
    pub errors: u64,
    pub restarts: u64,
    pub consecutive_failures: u32,
    pub last_call_ms: u64,
    pub last_error: Option<String>,
}

struct Running {
    child: Child,
    stdin: Option<ChildStdin>,
    frames: Receiver<Inbound>,
    server: ServerInfo,
    pid: u32,
}

/// The supervised backend.
pub struct Backend {
    config: BackendConfig,
    running: Option<Running>,
    stats: Stats,
    /// Bumped whenever a process is started, so the panel can show uptime.
    last_start: Option<Instant>,
    /// Process group of a leader that exited while descendants survived.
    ///
    /// `uvx` exec's the Python entry point, so a successful start can leave our
    /// direct child gone and its children running. When that happens the pid is
    /// kept here and reaped before the next start (and on teardown), so a reload
    /// cannot accumulate orphaned backends.
    orphan_group: Option<i32>,
}

impl Backend {
    pub fn new(config: BackendConfig) -> Self {
        Self {
            config,
            running: None,
            stats: Stats::default(),
            last_start: None,
            orphan_group: None,
        }
    }

    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    /// True when a backend process is alive right now.
    ///
    /// Uses `try_wait` rather than a flag, so a process that died while idle is
    /// reported honestly instead of looking healthy until the next call.
    ///
    /// The `Running` handle is dropped when the leader exits, but the **group id**
    /// is remembered: a leader that exited can still have live descendants in its
    /// process group (that is exactly what `uvx` does), and teardown has to know
    /// about them. Forgetting the pgid here is what leaked `search-mcp`.
    pub fn is_running(&mut self) -> bool {
        match self.running.as_mut() {
            Some(r) => match r.child.try_wait() {
                Ok(None) => true,
                Ok(Some(_)) | Err(_) => {
                    // The leader is gone. Keep its pid as the group to reap, drop
                    // the dead pipes, and let the next call start cleanly.
                    #[cfg(unix)]
                    {
                        let pgid = r.child.id() as i32;
                        if group_alive(pgid) {
                            self.orphan_group = Some(pgid);
                        } else {
                            self.orphan_group = None;
                        }
                    }
                    drop(r.stdin.take());
                    self.running = None;
                    false
                }
            },
            None => false,
        }
    }

    pub fn uptime(&mut self) -> Option<Duration> {
        if !self.is_running() {
            return None;
        }
        self.last_start.map(|t| t.elapsed())
    }

    pub fn pid(&mut self) -> Option<u32> {
        self.is_running().then(|| self.running.as_ref().map(|r| r.pid))?
    }

    pub fn server_info(&self) -> Option<&ServerInfo> {
        self.running.as_ref().map(|r| &r.server)
    }

    /// Start the backend if it is not running. Cheap and idempotent.
    pub fn ensure_started(&mut self) -> Result<(), McpError> {
        if self.is_running() {
            return Ok(());
        }
        if self.stats.consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
            return Err(McpError::Transport(format!(
                "it has failed {MAX_CONSECUTIVE_FAILURES} times in a row, so it is not being \
                 restarted automatically. Fix the backend (or reload the plugin) and call again"
            )));
        }
        match self.start() {
            Ok(()) => {
                self.stats.restarts += 1;
                Ok(())
            }
            Err(e) => {
                self.stats.consecutive_failures += 1;
                self.stats.last_error = Some(e.to_string());
                Err(e)
            }
        }
    }

    /// Spawn the process and run the MCP handshake.
    fn start(&mut self) -> Result<(), McpError> {
        self.stop_process();
        // A previous plugin process may have been killed without running its
        // teardown (a reload does exactly that). Its backend is recorded on disk.
        reap_recorded_groups();
        let (program, args) = self
            .config
            .command
            .split_first()
            .ok_or_else(|| McpError::Spawn("the configured `command` is empty".into()))?;

        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // The backend logs to stderr; draining it is not optional — a full
            // pipe would block the child mid-search.
            .stderr(Stdio::piped());
        for (key, value) in &self.config.env {
            command.env(key, value);
        }
        // Its own process group, so stopping it takes the Python child `uvx`
        // starts with it rather than orphaning it.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        #[cfg(target_os = "linux")]
        unsafe {
            use std::os::unix::process::CommandExt;
            let parent = std::process::id() as i32;
            command.pre_exec(move || {
                // SAFETY: `prctl` with PR_SET_PDEATHSIG is async-signal-safe and
                // takes no pointers; this runs in the forked child before exec.
                extern "C" {
                    fn prctl(option: i32, ...) -> i32;
                    fn getppid() -> i32;
                }
                const PR_SET_PDEATHSIG: i32 = 1;
                const SIGKILL: i32 = 9;
                prctl(PR_SET_PDEATHSIG, SIGKILL as std::ffi::c_ulong);
                // If the plugin died between fork and here, the signal already
                // fired at a parent that no longer exists; fail fast.
                if getppid() != parent {
                    std::process::exit(1);
                }
                Ok(())
            });
        }

        let mut child = command.spawn().map_err(|e| {
            McpError::Spawn(format!("`{}`: {e}", self.config.command_line()))
        })?;
        let pid = child.id();
        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Spawn("the backend has no stdout".into()))?;
        let stderr = child.stderr.take();

        // Drain stderr on its own thread, forwarded to ours so the host's log
        // has it. Bounded so a noisy backend cannot flood the log.
        if let Some(stderr) = stderr {
            std::thread::Builder::new()
                .name("free-search-stderr".into())
                .spawn(move || forward_stderr(stderr))
                .ok();
        }

        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("free-search-reader".into())
            .spawn(move || read_frames(stdout, tx))
            .map_err(|e| McpError::Spawn(format!("could not start the reader thread: {e}")))?;

        #[cfg(unix)]
        record_group(pid as i32);

        let mut running = Running {
            child,
            stdin,
            frames: rx,
            server: ServerInfo::default(),
            pid,
        };

        let handshake = self.handshake(&mut running);
        match handshake {
            Ok(server) => {
                running.server = server;
                self.running = Some(running);
                self.last_start = Some(Instant::now());
                // A successful start clears the strike count: failures are
                // consecutive, not cumulative.
                self.stats.consecutive_failures = 0;
                Ok(())
            }
            Err(e) => {
                kill_group(&mut running.child, running.stdin.take(), Duration::from_millis(250));
                Err(e)
            }
        }
    }

    fn handshake(&mut self, running: &mut Running) -> Result<ServerInfo, McpError> {
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "karucode-free-search-plugin", "version": env!("CARGO_PKG_VERSION")},
        });
        let reply = match request(running, "initialize", params, self.config.start_timeout) {
            Ok(reply) => reply,
            // A backend that never answers `initialize` is not a slow search, so
            // the "narrow the request" advice would be misleading: nothing about
            // the request is wrong, the process is.
            Err(McpError::Timeout { budget, .. }) => {
                return Err(McpError::Handshake(format!(
                    "no answer to `initialize` within {}",
                    human_duration(budget)
                )))
            }
            Err(e) => return Err(e),
        };
        let server = ServerInfo {
            name: reply
                .pointer("/serverInfo/name")
                .and_then(Value::as_str)
                .unwrap_or("search-mcp")
                .to_string(),
            version: reply
                .pointer("/serverInfo/version")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
            instructions: reply
                .get("instructions")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        };
        // Mandatory in the spec, and the backend starts serving tools after it.
        notify(running, "notifications/initialized", json!({}))?;
        Ok(server)
    }

    /// Call one backend tool.
    pub fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value, McpError> {
        let started = Instant::now();
        let outcome = match self.ensure_started() {
            Ok(()) => {
                let Some(running) = self.running.as_mut() else {
                    return Err(McpError::Transport("the backend is not running".into()));
                };
                let params = json!({"name": name, "arguments": arguments});
                request(running, "tools/call", params, self.config.call_timeout)
            }
            Err(e) => Err(e),
        };
        self.stats.last_call_ms = started.elapsed().as_millis() as u64;
        match &outcome {
            Ok(_) => {
                self.stats.calls += 1;
                self.stats.consecutive_failures = 0;
            }
            Err(e) => {
                self.stats.errors += 1;
                self.stats.last_error = Some(e.to_string());
                if e.is_backend_failure() {
                    self.stats.consecutive_failures += 1;
                    // A timeout or a broken pipe leaves a process we cannot
                    // trust; stop it so the next call starts a fresh one.
                    if matches!(e, McpError::Timeout { .. } | McpError::Transport(_)) {
                        self.stop_process();
                    }
                }
            }
        }
        outcome
    }

    /// Ask the backend which tools it has. Needs a live process.
    pub fn list_tools(&mut self) -> Result<Vec<BackendTool>, McpError> {
        self.ensure_started()?;
        let Some(running) = self.running.as_mut() else {
            return Err(McpError::Transport("the backend is not running".into()));
        };
        let reply = request(
            running,
            "tools/list",
            json!({}),
            self.config.start_timeout,
        )?;
        let tools = reply
            .get("tools")
            .and_then(Value::as_array)
            .ok_or_else(|| McpError::Protocol("tools/list carried no `tools` array".into()))?;
        Ok(tools
            .iter()
            .map(|t| BackendTool {
                name: t
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                description_len: t
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::len)
                    .unwrap_or(0),
                schema: t
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
            })
            .collect())
    }

    /// Stop the backend process, if any. Safe to call repeatedly.
    pub fn stop(&mut self) {
        self.stop_process();
    }

    fn stop_process(&mut self) {
        if let Some(mut running) = self.running.take() {
            kill_group(
                &mut running.child,
                running.stdin.take(),
                Duration::from_millis(1_500),
            );
            // The group is gone; a later process must not try to reap it again.
            #[cfg(unix)]
            forget_group(running.child.id() as i32);
        }
        // A leader that already exited can still have descendants in its group;
        // `is_running` parked that pgid rather than losing it.
        self.reap_orphan_group();
    }

    /// Kill a group left behind by a leader that exited.
    ///
    /// Called before every start and on teardown, so a plugin reload cannot
    /// accumulate orphaned backends. Each round is `kill(-pgid, TERM)`, wait up
    /// to a second, then `kill(-pgid, KILL)`: the same escalation a live child
    /// gets, because these descendants are the same processes.
    fn reap_orphan_group(&mut self) {
        #[cfg(unix)]
        if let Some(pgid) = self.orphan_group.take() {
            if !group_alive(pgid) {
                return;
            }
            unsafe {
                libc_kill(-pgid, 15);
            }
            for _ in 0..40 {
                if !group_alive(pgid) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            unsafe {
                libc_kill(-pgid, 9);
            }
        }
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        self.stop_process();
    }
}

/// Send a request and wait for its response, or the deadline.
fn request(
    running: &mut Running,
    method: &str,
    params: Value,
    budget: Duration,
) -> Result<Value, McpError> {
    static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
    write_line(running, &line)?;

    let deadline = Instant::now() + budget;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(McpError::Timeout {
                what: method.to_string(),
                budget,
            });
        }
        match running.frames.recv_timeout(remaining) {
            Ok(Inbound::Frame(frame)) => {
                // Notifications (progress pings) carry no id and are not the
                // answer; only a frame with our id ends the wait.
                if frame.get("id").and_then(Value::as_u64) != Some(id) {
                    continue;
                }
                if let Some(error) = frame.get("error") {
                    return Err(McpError::Remote {
                        code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                        message: error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("the backend reported an error")
                            .to_string(),
                    });
                }
                return Ok(frame.get("result").cloned().unwrap_or(Value::Null));
            }
            Ok(Inbound::Eof(reason)) => return Err(McpError::Transport(reason)),
            Err(RecvTimeoutError::Timeout) => {
                return Err(McpError::Timeout {
                    what: method.to_string(),
                    budget,
                })
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(McpError::Transport(
                    "the reader thread for the backend stopped".into(),
                ))
            }
        }
    }
}

fn notify(running: &mut Running, method: &str, params: Value) -> Result<(), McpError> {
    let line = json!({"jsonrpc": "2.0", "method": method, "params": params});
    write_line(running, &line)
}

fn write_line(running: &mut Running, value: &Value) -> Result<(), McpError> {
    let stdin = running
        .stdin
        .as_mut()
        .ok_or_else(|| McpError::Transport("the backend's stdin is closed".into()))?;
    let mut line = value.to_string();
    line.push('\n');
    stdin
        .write_all(line.as_bytes())
        .and_then(|()| stdin.flush())
        .map_err(|e| McpError::Transport(format!("could not write to the backend: {e}")))
}

/// Read newline-delimited JSON from the child until it closes stdout.
fn read_frames(stdout: ChildStdout, tx: Sender<Inbound>) {
    let mut reader = BufReader::new(stdout);
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        match read_line_capped(&mut reader, &mut buf, MAX_LINE_BYTES) {
            Ok(false) => {
                let _ = tx.send(Inbound::Eof(
                    "the backend closed its output (it exited or crashed)".into(),
                ));
                return;
            }
            Ok(true) => {}
            Err(e) => {
                let _ = tx.send(Inbound::Eof(format!("the backend broke framing: {e}")));
                return;
            }
        }
        let text = String::from_utf8_lossy(&buf);
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(trimmed) {
            Ok(frame) => {
                if tx.send(Inbound::Frame(frame)).is_err() {
                    return; // The plugin dropped the receiver: we are shutting down.
                }
            }
            Err(e) => {
                // Anything that is not JSON on stdout is the backend's bug. Report
                // it rather than silently skipping a frame that might be the answer.
                let _ = tx.send(Inbound::Eof(format!(
                    "the backend wrote a non-JSON line ({e}): {}",
                    trimmed.chars().take(200).collect::<String>()
                )));
                return;
            }
        }
    }
}

/// Read one line, refusing to buffer more than `cap` bytes for it.
///
/// `read_line` would grow the buffer without bound on a backend that emits one
/// enormous line; this returns `Ok(false)` at EOF and an error past the cap.
fn read_line_capped(
    reader: &mut BufReader<ChildStdout>,
    buf: &mut Vec<u8>,
    cap: usize,
) -> std::io::Result<bool> {
    loop {
        let available = match reader.fill_buf() {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if available.is_empty() {
            return Ok(!buf.is_empty());
        }
        let (done, used) = match available.iter().position(|&b| b == b'\n') {
            Some(i) => {
                buf.extend_from_slice(&available[..=i]);
                (true, i + 1)
            }
            None => {
                buf.extend_from_slice(available);
                (false, available.len())
            }
        };
        reader.consume(used);
        if buf.len() > cap {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("one line exceeded {cap} bytes"),
            ));
        }
        if done {
            return Ok(true);
        }
    }
}

/// Forward the backend's stderr to ours, bounded.
fn forward_stderr(stderr: std::process::ChildStderr) {
    const MAX_LINES: usize = 200;
    let mut reader = BufReader::new(stderr);
    let mut line = String::new();
    for seen in 0.. {
        line.clear();
        if !matches!(reader.read_line(&mut line), Ok(n) if n > 0) {
            return;
        }
        if seen >= MAX_LINES {
            eprintln!("[free-search-backend] further output suppressed");
            // Keep draining so the child never blocks on a full pipe, but stop
            // forwarding: a backend that logs per request would flood our log.
            let mut sink = String::new();
            while let Ok(n) = reader.read_line(&mut sink) {
                if n == 0 {
                    break;
                }
                sink.clear();
            }
            return;
        }
        if !line.trim().is_empty() {
            eprint!("[free-search-backend] {line}");
        }
    }
}

/// Close stdin, then signal the whole process group, escalating to SIGKILL.
///
/// Closing stdin is the graceful path: an MCP stdio server exits on EOF. The group
/// signal is what catches the *descendants* — the Python child `uvx` spawns, and
/// the Playwright Chromium tree that child spawns — which would otherwise be
/// orphaned and keep the cache profile busy.
///
/// # The bug this is written around
///
/// An earlier version did `if child.try_wait() == Some(_) { return; }` once the
/// direct child had exited, and that leaked. The exit of the direct child says
/// nothing about the group: when `uvx` exec's the entry point the group leader
/// *becomes* the Python process, and when it is the Python process that dies, its
/// Playwright children are reparented to init and keep running. Observed once for
/// real: the recipe check tore down the plugin's direct child and left
/// `search-mcp` plus an eleven-process Chromium tree alive for eight seconds. So
/// the group is signalled whenever it still has members, and `kill(-pgid, 0)` is
/// the probe that answers that.
fn kill_group(child: &mut Child, stdin: Option<ChildStdin>, grace: Duration) {
    // Closing stdin is the graceful signal; do it first and let the loop below
    // notice the exit. `wait()` must NOT be called before the group signal: it
    // blocks until the direct child dies, and a backend stuck in a long call
    // ignores EOF, so waiting first deadlocks for the length of that call. That
    // bug turned a 400 ms timeout test into a 30 s one.
    drop(stdin);
    let pid = child.id() as i32;
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => break,
        }
    }

    #[cfg(unix)]
    {
        // The exit of the direct child says nothing about descendants: when `uvx`
        // execs the entry point the group leader *becomes* the Python process, and
        // when that process dies its Playwright children are reparented to init
        // and keep running. Observed once for real (eleven Chromium processes
        // survived a teardown), so the group is signalled whenever it still has
        // members.
        if group_alive(-pid) {
            // SIGTERM first: the backend flushes its SQLite cache and exits.
            // SAFETY: a negative pid signals the process group created with
            // `process_group(0)`; a stale group is a harmless ESRCH.
            unsafe {
                libc_kill(-pid, 15);
            }
            for _ in 0..20 {
                if !group_alive(-pid) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            if group_alive(-pid) {
                unsafe {
                    libc_kill(-pid, 9);
                }
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
    // Reap the direct child now that the group is gone, so it does not linger as
    // a zombie for the life of the plugin process.
    let _ = child.wait();
}

/// Directory holding one record per live backend, keyed by the group leader's pid.
///
/// # Why a directory, not a single file
///
/// A first attempt used one `backend.pgid`. That is wrong as soon as two plugin
/// processes exist — a development copy, a probe script, two karucode servers —
/// because whichever starts last overwrites the record and the other process's
/// backend becomes unreapable. Observed for real: `probe/drive_plugin.py` and the
/// installed plugin clobbered each other and an orphan slipped through.
///
/// So each record is named after the group it describes, and carries the pid of
/// the plugin process that owns it:
///
/// ```text
/// $XDG_CACHE_HOME/karucode-free-search/backends/<pgid>   -> "<owner pid>"
/// ```
///
/// A process reaps a record only when its owner is gone, so a live plugin's
/// backend is never pulled out from under it.
fn state_dir() -> std::path::PathBuf {
    let base = std::env::var("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| std::path::PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|_| std::env::temp_dir());
    base.join("karucode-free-search").join("backends")
}

fn record_path(pgid: i32) -> std::path::PathBuf {
    state_dir().join(pgid.to_string())
}

/// Record that `pgid` belongs to this plugin process.
fn record_group(pgid: i32) {
    let path = record_path(pgid);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // The owner pid is what tells a later process whether this record is stale.
    let _ = std::fs::write(&path, std::process::id().to_string());
}

/// Drop the record for a group we stopped ourselves.
fn forget_group(pgid: i32) {
    let _ = std::fs::remove_file(record_path(pgid));
}

/// Whether `pid` is still a live process (not merely a zombie).
fn process_alive(pid: i32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .map(|state| state != "Z")
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// Reap backend groups left behind by plugin processes that are gone.
///
/// A reload replaces the plugin *process*, so the old process is killed without
/// running its teardown and its backend survives. This is what cleans that up:
/// every record whose owner is dead and whose group still exists is signalled,
/// with the same guard as [`group_looks_like_a_backend`] so a recycled pid can
/// never be killed.
fn reap_recorded_groups() {
    let Ok(entries) = std::fs::read_dir(state_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(pgid) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.parse::<i32>().ok())
        else {
            let _ = std::fs::remove_file(&path);
            continue;
        };
        let owner = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| t.trim().parse::<i32>().ok());
        let owner_alive = owner.map(process_alive).unwrap_or(false);
        let mine = owner == Some(std::process::id() as i32);

        // A live owner keeps its backend, whichever process it is. For our own
        // records that is not bookkeeping pedantry: a restart after a timeout calls
        // this while the group we are abandoning may still be shutting down, and
        // pulling it out from under ourselves would race the teardown that is
        // already in flight.
        if owner_alive {
            continue;
        }

        // The owner is gone. Deal with the group *before* dropping the record:
        // deleting first and then declining to act loses the only pointer to a live
        // orphan, which is exactly how three untracked backends survived a stress
        // run.
        if mine || pgid <= 1 || !group_alive(pgid) {
            // Nothing to kill: our own group, a nonsense id, or a group that is
            // already gone. The record has served its purpose.
            let _ = std::fs::remove_file(&path);
            continue;
        }
        if !group_looks_like_a_backend(pgid) {
            // Leave the record in place: a recycled pid may look foreign now and
            // like ours in a moment, and losing the pointer is worse than a stale
            // file. Reported so it is visible rather than silent.
            eprintln!(
                "[free-search] not reaping process group {pgid}: its members are not search \
                 backends (keeping the record for a later attempt)"
            );
            continue;
        }
        eprintln!(
            "[free-search] reaping backend group {pgid} left behind by plugin process {:?}",
            owner
        );
        #[cfg(unix)]
        unsafe {
            libc_kill(-pgid, 15);
        }
        for _ in 0..40 {
            if !group_alive(pgid) {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        if group_alive(pgid) {
            #[cfg(unix)]
            unsafe {
                libc_kill(-pgid, 9);
            }
        }
        // Only now, once the group is gone, is the record redundant.
        if !group_alive(pgid) {
            let _ = std::fs::remove_file(&path);
        } else {
            eprintln!(
                "[free-search] backend group {pgid} survived SIGKILL; keeping its record so a \
                 later start retries"
            );
        }
    }
}

/// Whether `group` is safe to signal: at least one member is our backend and no
/// member is something unrelated.
///
/// This is the guard that makes killing a pid remembered on disk safe, and the
/// first version of it was too strict to work. It required *every* member's
/// command line to name the backend, and a real backend group is not like that:
/// `search-mcp` runs beside Playwright's `node` driver, whose command line says
/// `.../playwright/driver/node` and nothing else. So the guard refused every real
/// group, the reaper logged "not search backends" forever, and orphans piled up
/// while the code looked correct.
///
/// The rule that matches reality:
///
/// * at least one non-zombie member must name the search backend (proof this is
///   our group, not a recycled pid);
/// * no non-zombie member may be recognisably unrelated (a shell, an editor, a
///   database) — the members that are neither ours nor recognisable, like the
///   Playwright helper, are the backend's own children and go with it.
fn group_looks_like_a_backend(group: i32) -> bool {
    let mut ours = 0;
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.parse::<i32>().is_err() {
            continue;
        }
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{name}/stat")) else {
            continue;
        };
        let Some(rest) = stat.rsplit_once(')').map(|(_, rest)| rest) else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        let state = fields.next().unwrap_or("");
        let _ppid = fields.next();
        let Some(pgrp) = fields.next().and_then(|v| v.parse::<i32>().ok()) else {
            continue;
        };
        if pgrp != group {
            continue;
        }
        // A zombie has no command line and no descendants left to worry about.
        if state == "Z" {
            continue;
        }
        let Ok(cmdline) = std::fs::read(format!("/proc/{name}/cmdline")) else {
            continue;
        };
        let cmdline = String::from_utf8_lossy(&cmdline).replace('\0', " ");
        if cmdline.trim().is_empty() {
            continue;
        }
        if cmdline.contains("search-mcp")
            || cmdline.contains("free-search-mcp")
            || cmdline.contains("uvx")
            || cmdline.contains("playwright")
        {
            if cmdline.contains("search-mcp") || cmdline.contains("free-search-mcp") {
                ours += 1;
            }
            continue;
        }
        // Anything else in the group means this is not (only) our backend.
        return false;
    }
    ours > 0
}

/// Whether any process still belongs to `group` (`pid` of the group leader).
///
/// `kill(-pgid, 0)` is **not** enough, and this is the subtlety that leaked
/// `search-mcp` and its Chromium tree in production. That call signals the
/// members currently registered in the group; once the leader has exited and a
/// child has been reparented to init, the kernel often reports the group as gone
/// while the child is still alive and still its own group leader. So the group is
/// enumerated directly from `/proc`, which is the only authoritative answer.
#[cfg(unix)]
fn group_alive(group: i32) -> bool {
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.parse::<i32>().is_err() {
                continue;
            }
            // `/proc/<pid>/stat`: `pid (comm) state ppid pgrp ...`. `comm` may
            // contain spaces and parentheses, so split after the LAST ')'.
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{name}/stat")) else {
                continue;
            };
            let Some(rest) = stat.rsplit_once(')').map(|(_, rest)| rest) else {
                continue;
            };
            let mut fields = rest.split_whitespace();
            let _state = fields.next();
            let _ppid = fields.next();
            let Some(pgrp) = fields.next().and_then(|v| v.parse::<i32>().ok()) else {
                continue;
            };
            if pgrp == group {
                return true;
            }
        }
    }
    // Fall back to the syscall if /proc is not mounted on this platform.
    unsafe { libc_kill(group, 0) == 0 }
}

/// `kill(2)` without pulling in the `libc` crate.
#[cfg(unix)]
unsafe fn libc_kill(pid: i32, signal: i32) -> i32 {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    kill(pid, signal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn cfg(command: Vec<&str>) -> BackendConfig {
        BackendConfig {
            command: command.into_iter().map(str::to_string).collect(),
            env: Default::default(),
            start_timeout: Duration::from_secs(5),
            call_timeout: Duration::from_secs(5),
        }
    }

    /// A stand-in MCP server: answers the handshake, echoes `tools/call`.
    ///
    /// Written as a shell script so the tests exercise the real spawn, pipe and
    /// framing path rather than a mocked one.
    fn fake_server(dir: &std::path::Path) -> std::path::PathBuf {
        let path = dir.join("fake-mcp.sh");
        // `id=$(id_of "$line")` and echoing it back is what a real server does;
        // hardcoding an id works only while tests run one at a time, because the
        // client's id counter is process-global.
        let script = r#"#!/bin/sh
id_of() { printf '%s' "$1" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p'; }
while IFS= read -r line; do
  id=$(id_of "$line")
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"protocolVersion\":\"2025-06-18\",\"serverInfo\":{\"name\":\"search-mcp\",\"version\":\"0.17.0\"},\"instructions\":\"be careful\"}}"
      ;;
    *'"method":"tools/list"'*)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"tools\":[{\"name\":\"search\",\"description\":\"d\",\"inputSchema\":{\"type\":\"object\",\"properties\":{}}}]}}"
      ;;
    *'"method":"tools/call"'*)
      # A progress notification first: the client must skip it, not mistake it
      # for the answer.
      printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info"}}'
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"ok\"}],\"isError\":false}}"
      ;;
    *'"method":"notifications/initialized"'*) : ;;
  esac
done
"#;
        std::fs::write(&path, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kc-free-search-test-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Serialize the tests that write an executable and then spawn it.
    ///
    /// Without this the suite fails at random with `ETXTBSY` ("Text file busy"),
    /// which looks like a plugin bug and is not one: `fork` duplicates every open
    /// file descriptor, so if thread A is mid-`fs::write` on a script while thread
    /// B forks to run *its* script, B's child inherits a write handle on A's file
    /// and A's `exec` is refused. Linux only; it is why the failure moves between
    /// tests, and why it never happens with `--test-threads=1`.
    ///
    /// The plugin itself cannot hit this: it never writes an executable at
    /// runtime. Serializing here is the honest fix — retrying on `ETXTBSY` would
    /// hide the fact that the fixture creates the race.
    fn exec_lock() -> &'static Mutex<()> {
        static LOCK: Mutex<()> = Mutex::new(());
        &LOCK
    }

    #[test]
    fn a_missing_binary_is_a_spawn_error_that_names_the_command() {
        let mut backend = Backend::new(cfg(vec!["/nonexistent/binary-xyz"]));
        let err = backend.ensure_started().unwrap_err();
        assert!(matches!(err, McpError::Spawn(_)), "{err:?}");
        let text = err.to_string();
        assert!(text.contains("/nonexistent/binary-xyz"), "{text}");
        assert!(text.contains("uvx"), "the remedy must mention the default: {text}");
        assert!(!backend.is_running());
    }

    #[test]
    fn an_empty_command_is_refused_rather_than_panicking() {
        let mut backend = Backend::new(cfg(vec![]));
        let err = backend.ensure_started().unwrap_err();
        assert!(matches!(err, McpError::Spawn(_)));
    }

    #[test]
    fn handshake_call_and_list_work_against_a_real_process() {
        let _exec = exec_lock().lock().unwrap_or_else(|e| e.into_inner());
        let dir = tmpdir("happy");
        let mut server = cfg(vec![fake_server(&dir).to_str().unwrap()]);
        server.start_timeout = Duration::from_secs(5);
        let mut backend = Backend::new(server);
        backend.ensure_started().expect("handshake");
        assert!(backend.is_running());
        assert_eq!(backend.server_info().unwrap().version, "0.17.0");
        assert_eq!(backend.server_info().unwrap().instructions, "be careful");

        // The progress notification the fake server emits before the answer must
        // not be mistaken for the response.
        let result = backend
            .call_tool("search", json!({"query": "x"}))
            .expect("call");
        assert_eq!(result["content"][0]["text"], "ok");
        assert_eq!(backend.stats().calls, 1);

        let tools = backend.list_tools().expect("tools/list");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "search");
        backend.stop();
        assert!(!backend.is_running());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_jsonrpc_error_is_remote_not_a_backend_failure() {
        let _exec = exec_lock().lock().unwrap_or_else(|e| e.into_inner());
        // A refusal of the arguments means the backend is healthy: it must not
        // count towards the restart budget.
        let dir = tmpdir("remote");
        let path = dir.join("err-mcp.sh");
        std::fs::write(
            &path,
            r#"#!/bin/sh
id_of() { printf '%s' "$1" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p'; }
while IFS= read -r line; do
  id=$(id_of "$line")
  case "$line" in
    *'"method":"initialize"'*) printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"serverInfo\":{\"name\":\"s\",\"version\":\"1\"}}}" ;;
    *'"method":"tools/call"'*) printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"error\":{\"code\":-32000,\"message\":\"query must not be empty\"}}" ;;
  esac
done
"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut backend = Backend::new(cfg(vec![path.to_str().unwrap()]));
        let err = backend.call_tool("search", json!({})).unwrap_err();
        assert!(matches!(err, McpError::Remote { .. }), "{err:?}");
        assert!(err.to_string().contains("query must not be empty"));
        assert!(!err.is_backend_failure());
        assert_eq!(backend.stats().consecutive_failures, 0);
        // The process is still usable: a remote error does not kill it.
        assert!(backend.is_running());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dead_backend_is_restarted_on_the_next_call() {
        let _exec = exec_lock().lock().unwrap_or_else(|e| e.into_inner());
        // The first process exits after one call; the next call must start a new
        // one instead of failing forever (which is what the plain MCP path does).
        let dir = tmpdir("restart");
        let path = dir.join("once-mcp.sh");
        std::fs::write(
            &path,
            r#"#!/bin/sh
id_of() { printf '%s' "$1" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p'; }
while IFS= read -r line; do
  id=$(id_of "$line")
  case "$line" in
    *'"method":"initialize"'*) printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"serverInfo\":{\"name\":\"s\",\"version\":\"1\"}}}" ;;
    *'"method":"tools/call"'*)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"done\"}]}}"
      exit 0
      ;;
  esac
done
"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut backend = Backend::new(cfg(vec![path.to_str().unwrap()]));
        let first = backend.call_tool("search", json!({}));
        assert!(first.is_ok(), "first call failed: {first:?}");
        // The process exited with the answer; `is_running` must notice.
        std::thread::sleep(Duration::from_millis(100));
        assert!(!backend.is_running());
        let second = backend.call_tool("search", json!({}));
        assert!(second.is_ok(), "second call did not restart it: {second:?}");
        assert_eq!(backend.stats().calls, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_hung_backend_times_out_with_the_configured_budget() {
        let _exec = exec_lock().lock().unwrap_or_else(|e| e.into_inner());
        let dir = tmpdir("timeout");
        let path = dir.join("hang-mcp.sh");
        std::fs::write(
            &path,
            r#"#!/bin/sh
id_of() { printf '%s' "$1" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p'; }
while IFS= read -r line; do
  id=$(id_of "$line")
  case "$line" in
    *'"method":"initialize"'*) printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"serverInfo\":{\"name\":\"s\",\"version\":\"1\"}}}" ;;
    *'"method":"tools/call"'*) sleep 30 ;;
  esac
done
"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut config = cfg(vec![path.to_str().unwrap()]);
        config.call_timeout = Duration::from_millis(400);
        let mut backend = Backend::new(config);
        let started = Instant::now();
        let err = backend.call_tool("search", json!({})).unwrap_err();
        let elapsed = started.elapsed();
        assert!(matches!(err, McpError::Timeout { .. }), "{err:?}");
        // The message must name the remedy, not just the deadline.
        assert!(err.to_string().contains("Narrow the request"), "{err}");
        assert!(elapsed < Duration::from_secs(5), "took {elapsed:?}");
        // A timed-out process is stopped rather than left running.
        assert!(!backend.is_running());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stopping_the_backend_also_kills_its_descendants() {
        // The leak this exists for: `uvx` spawns the Python entry point as a
        // child, and that child spawns a Playwright Chromium tree. Killing only
        // the direct child leaves the grandchildren running, reparented to init.
        //
        // The fixture records its own subshell's pid, so the test can check the
        // *grandchild* — the process a real teardown leaked.
        let _exec = exec_lock().lock().unwrap_or_else(|e| e.into_inner());
        let dir = tmpdir("descendants");
        let pidfile = dir.join("grandchild.pid");
        let path = dir.join("spawner.sh");
        // Quote the path: `tmpdir` embeds `ThreadId(13)`, and an unquoted path
        // with parentheses is a shell syntax error.
        std::fs::write(
            &path,
            format!(
                r#"#!/bin/sh
# A long-lived "Playwright" stand-in: one level below the group leader.
sleep 120 &
echo $! > "{}"
id_of() {{ printf '%s' "$1" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p'; }}
while IFS= read -r line; do
  id=$(id_of "$line")
  case "$line" in
    *'"method":"initialize"'*) printf '%s\n' "{{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{{\"serverInfo\":{{\"name\":\"s\",\"version\":\"1\"}}}}}}" ;;
  esac
done
"#,
                pidfile.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let mut backend = Backend::new(cfg(vec![path.to_str().unwrap()]));
        backend.ensure_started().expect("handshake");

        // Wait for the fixture to record the grandchild.
        let mut grandchild = None;
        for _ in 0..100 {
            if let Ok(text) = std::fs::read_to_string(&pidfile) {
                if let Ok(pid) = text.trim().parse::<i32>() {
                    grandchild = Some(pid);
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let grandchild = grandchild.expect("the fixture must spawn and record a child");
        assert!(group_alive(grandchild), "the grandchild should be running");

        backend.stop();

        // It must be gone. `kill(-pgid, 0)` goes through init for a process that
        // was reparented, so this is checked against /proc directly.
        for _ in 0..100 {
            if !group_alive(grandchild) {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(
            !group_alive(grandchild),
            "grandchild {grandchild} survived the teardown: that is the orphaned \
             Chromium tree"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_backend_whose_leader_exited_still_has_its_group_killed() {
        // The real shape of `uvx`: it starts, spawns the Python entry point, and
        // then exits — leaving the grandchild alive in the same process group.
        // Observed for real: three `search-mcp` orphans (plus a Chromium tree)
        // survived plugin reloads for minutes. The earlier teardown only signalled
        // the group while `kill(-pgid, 0)` still reported members, so this fixture
        // pins that path: leader gone, group non-empty.
        let _exec = exec_lock().lock().unwrap_or_else(|e| e.into_inner());
        let dir = tmpdir("leader-exits");
        let pidfile = dir.join("survivor.pid");
        let path = dir.join("uvx-like.sh");
        std::fs::write(
            &path,
            format!(
                r#"#!/bin/sh
# Answer the handshake, spawn a long-lived "Python entry point", then exit like
# `uvx` does once the real process is running.
id_of() {{ printf '%s' "$1" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p'; }}
IFS= read -r line
id=$(id_of "$line")
printf '%s\n' "{{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{{\"serverInfo\":{{\"name\":\"s\",\"version\":\"1\"}}}}}}"
sleep 120 &
echo $! > "{}"
exit 0
"#,
                pidfile.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let mut backend = Backend::new(cfg(vec![path.to_str().unwrap()]));
        backend.ensure_started().expect("handshake");

        let mut survivor = None;
        for _ in 0..100 {
            if let Ok(text) = std::fs::read_to_string(&pidfile) {
                if let Ok(pid) = text.trim().parse::<i32>() {
                    survivor = Some(pid);
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let survivor = survivor.expect("the fixture must spawn and record a survivor");
        // The leader (our direct child) is gone; the group is not.
        for _ in 0..100 {
            if !backend.is_running() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!backend.is_running(), "the leader should have exited");
        assert!(
            group_alive(survivor),
            "the survivor should still be alive before teardown"
        );

        backend.stop();

        for _ in 0..100 {
            if !group_alive(survivor) {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(
            !group_alive(survivor),
            "survivor {survivor} outlived the teardown: this is the orphaned \
             `search-mcp` (and Chromium) process"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_recorded_group_is_only_reaped_when_its_owner_is_gone_and_it_is_ours() {
        // The record is what lets a *replacement* plugin process reap a backend its
        // predecessor left behind on reload. It is also untrusted input (a recycled
        // pid, a shared cache dir, a hand-edited file), so every guard is tested:
        //
        //   (a) a live owner's record must never be touched;
        //   (b) a dead owner's non-backend group must not be killed;
        //   (c) a missing group is a no-op;
        //   (d) nonsense is cleaned up rather than retried forever.
        let _exec = exec_lock().lock().unwrap_or_else(|e| e.into_inner());
        let dir = state_dir();

        // (a) The owner is this test process, which is alive — the same case as a
        // second plugin process watching a live plugin's backend.
        let mut decoy = std::process::Command::new("sleep").arg("30").group_spawn();
        std::fs::create_dir_all(&dir).unwrap();
        let live_record = record_path(decoy.pgid());
        std::fs::write(&live_record, std::process::id().to_string()).unwrap();
        reap_recorded_groups();
        assert!(
            group_alive(decoy.pgid()),
            "a record owned by a live process must not be reaped"
        );
        assert!(live_record.exists(), "and its record must survive");
        let _ = std::fs::remove_file(&live_record);

        // (a2) Our own group is never reaped: a restart after a timeout relies on
        // that, since the backend we are abandoning may still be shutting down.
        std::fs::write(record_path(decoy.pgid()), std::process::id().to_string()).unwrap();
        reap_recorded_groups();
        assert!(group_alive(decoy.pgid()), "our own group must be left alone");
        let _ = std::fs::remove_file(record_path(decoy.pgid()));

        // (b) A dead owner's record pointing at a live group that is *not* a
        // backend: the guard must refuse to kill it, and must keep the record —
        // deleting it would throw away the only pointer to a real orphan if the
        // group merely looked foreign at this instant.
        let foreign = record_path(decoy.pgid());
        std::fs::write(&foreign, "999999").unwrap();
        assert!(!process_alive(999_999), "the recorded owner must look dead");
        assert!(!group_looks_like_a_backend(decoy.pgid()), "`sleep 30` is not a backend");
        reap_recorded_groups();
        assert!(group_alive(decoy.pgid()), "a non-backend group must never be killed");
        assert!(foreign.exists(), "the pointer to a live group must be kept");
        let _ = std::fs::remove_file(&foreign);

        // (b2) A dead owner whose group is already gone needs no record.
        let stale = record_path(999_997);
        std::fs::write(&stale, "999999").unwrap();
        reap_recorded_groups();
        assert!(!stale.exists(), "a record for a vanished group must be dropped");

        // (c) A dead owner whose group no longer exists.
        let gone = record_path(999_998);
        std::fs::write(&gone, "999999").unwrap();
        reap_recorded_groups();
        assert!(!gone.exists());

        // (d) A filename that is not a pid is removed rather than retried.
        let junk = dir.join("not-a-pid");
        std::fs::write(&junk, "x").unwrap();
        reap_recorded_groups();
        assert!(!junk.exists(), "a malformed record must not be left behind");

        decoy.kill();
        let _ = decoy.wait();
    }

    #[test]
    fn the_backend_guard_accepts_a_real_group_and_refuses_a_foreign_one() {
        // The first version of this guard required *every* member of the group to
        // name the backend, and real backend groups contain Playwright's node
        // driver (`.../playwright/driver/node`). So it rejected every genuine
        // orphan while looking correct, and the stress run piled up backends. This
        // pins both directions against live process groups.
        let _exec = exec_lock().lock().unwrap_or_else(|e| e.into_inner());

        // A group with a backend-named member is ours. `sh` here is dash, which
        // has no `exec -a`, so the fixture copies a tiny binary under a name
        // containing `search-mcp` and runs that: the predicate reads the full
        // cmdline, and argv[0] is what names it.
        let dir = tmpdir("guard");
        let fake = dir.join("search-mcp-stand-in");
        std::fs::copy("/bin/sleep", &fake).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut ours = std::process::Command::new(&fake).arg("30").group_spawn();
        assert!(
            group_looks_like_a_backend(ours.pgid()),
            "a group containing a backend process must be acceptable"
        );
        ours.kill();
        let _ = ours.wait();

        // A group of unrelated processes is not.
        let mut foreign = std::process::Command::new("sleep").arg("30").group_spawn();
        assert!(group_alive(foreign.pgid()));
        assert!(
            !group_looks_like_a_backend(foreign.pgid()),
            "a group of foreign processes must be refused"
        );
        foreign.kill();
        let _ = foreign.wait();

        // A group that does not exist is not ours either.
        assert!(!group_looks_like_a_backend(999_996));
    }

    #[test]
    fn process_liveness_is_judged_by_state_not_existence() {
        // A zombie still has /proc/<pid>, so `kill(pid, 0)` would report a dead
        // plugin as alive and its backend would never be reaped.
        assert!(process_alive(std::process::id() as i32), "this test is alive");
        assert!(!process_alive(999_999), "an unused pid is not alive");
    }

    /// Spawn a process in its own group, like the backend, and report its pgid.
    trait GroupSpawn {
        fn group_spawn(&mut self) -> GroupChild;
    }

    struct GroupChild(std::process::Child);
    impl GroupChild {
        fn pid(&self) -> i32 {
            self.0.id() as i32
        }
        /// Its own process group: `Command::process_group(0)` makes pid == pgid.
        fn pgid(&self) -> i32 {
            self.pid()
        }
        fn kill(&mut self) {
            let _ = self.0.kill();
        }
        fn wait(&mut self) {
            let _ = self.0.wait();
        }
    }
    impl GroupSpawn for std::process::Command {
        fn group_spawn(&mut self) -> GroupChild {
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                self.process_group(0);
            }
            GroupChild(self.spawn().expect("spawn"))
        }
    }

    #[test]
    fn a_backend_that_never_handshakes_fails_with_a_useful_message() {
        let _exec = exec_lock().lock().unwrap_or_else(|e| e.into_inner());
        let dir = tmpdir("silent");
        let path = dir.join("silent.sh");
        std::fs::write(&path, "#!/bin/sh\ncat > /dev/null\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut config = cfg(vec![path.to_str().unwrap()]);
        config.start_timeout = Duration::from_millis(300);
        let mut backend = Backend::new(config);
        let err = backend.ensure_started().unwrap_err();
        assert!(matches!(err, McpError::Handshake(_)), "{err:?}");
        // The message must name what failed and what to do about it, not advise
        // narrowing a search the model never made.
        let text = err.to_string();
        assert!(text.contains("handshake"), "{text}");
        assert!(text.contains("by hand"), "{text}");
        assert!(!text.contains("Narrow the request"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_json_output_is_an_error_not_a_silent_skip() {
        let _exec = exec_lock().lock().unwrap_or_else(|e| e.into_inner());
        let dir = tmpdir("garbage");
        let path = dir.join("garbage.sh");
        std::fs::write(
            &path,
            "#!/bin/sh\nprintf '%s\\n' 'not json at all'\ncat > /dev/null\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut backend = Backend::new(cfg(vec![path.to_str().unwrap()]));
        let err = backend.ensure_started().unwrap_err();
        assert!(matches!(err, McpError::Transport(_)), "{err:?}");
        assert!(err.to_string().contains("non-JSON line"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_backend_stops_being_retried_after_the_budget() {
        // Three strikes, then a message that says so instead of burning another
        // timeout on every call.
        let mut backend = Backend::new(cfg(vec!["/nonexistent/binary-xyz"]));
        for _ in 0..MAX_CONSECUTIVE_FAILURES {
            assert!(backend.call_tool("search", json!({})).is_err());
        }
        let err = backend.call_tool("search", json!({})).unwrap_err();
        assert!(err.to_string().contains("3 times in a row"), "{err}");
    }

    #[test]
    fn a_successful_call_clears_the_strike_count() {
        let _exec = exec_lock().lock().unwrap_or_else(|e| e.into_inner());
        // The budget is consecutive, so a long session survives one crash.
        let dir = tmpdir("strikes");
        let mut config = cfg(vec![fake_server(&dir).to_str().unwrap()]);
        config.start_timeout = Duration::from_secs(5);
        let mut backend = Backend::new(config);
        backend.stats.consecutive_failures = MAX_CONSECUTIVE_FAILURES - 1;
        assert!(backend.call_tool("search", json!({})).is_ok());
        assert_eq!(backend.stats().consecutive_failures, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
