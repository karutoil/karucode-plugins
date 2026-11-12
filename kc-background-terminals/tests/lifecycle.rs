#![cfg(unix)]
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(8);
struct Plugin {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    id: u64,
}
impl Plugin {
    fn spawn() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_kc-background-terminals"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if tx.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            lines,
            id: 0,
        }
    }
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc":"2.0", "id":self.id, "method":method, "params":params})
        )
        .unwrap();
        stdin.flush().unwrap();
        let line = self
            .lines
            .recv_timeout(WAIT)
            .expect("plugin response timed out");
        let response: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["id"], self.id, "{response}");
        assert!(response.get("error").is_none(), "{response}");
        response["result"].clone()
    }
    fn raw_call(&mut self, name: &str, args: Value, session: &str, workspace: &str) -> Value {
        self.request(
            "tools/call",
            json!({"name":name,"arguments":args,
            "context":{"workspace":workspace,"session_id":session}}),
        )
    }
    fn call(&mut self, name: &str, args: Value) -> Value {
        let result = self.raw_call(name, args, "integration", "/tmp");
        assert_eq!(result["ok"], true, "{result}");
        result["meta"].clone()
    }
    fn wait_exit(&mut self, success: bool) {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert_eq!(status.success(), success, "plugin exited: {status}");
                return;
            }
            assert!(Instant::now() < deadline, "plugin failed to exit");
            thread::sleep(Duration::from_millis(20));
        }
    }
}
impl Drop for Plugin {
    fn drop(&mut self) {
        self.stdin.take();
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn until(mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for condition");
        thread::sleep(Duration::from_millis(20));
    }
}
fn read_until(plugin: &mut Plugin, id: &str, predicate: impl Fn(&Value) -> bool) -> Value {
    let mut latest = Value::Null;
    until(|| {
        latest = plugin.call("terminal_read", json!({"id":id}));
        predicate(&latest)
    });
    latest
}
fn start_http(plugin: &mut Plugin) -> (String, SocketAddr) {
    // The server selects its own ephemeral port, avoiding fixed-port collisions.
    let cmd = "python3 -u -c 'import http.server; s=http.server.HTTPServer((\"127.0.0.1\",0),http.server.SimpleHTTPRequestHandler); print(s.server_port,flush=True); s.serve_forever()'";
    let start = plugin.call("terminal_start", json!({"command":cmd}));
    assert_eq!(start["cwd"], "/tmp");
    let id = start["id"].as_str().unwrap().to_owned();
    let output = read_until(plugin, &id, |v| {
        v["output"].as_str().unwrap().contains('\n')
    });
    let port: u16 = output["output"]
        .as_str()
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    (id, ([127, 0, 0, 1], port).into())
}
fn assert_closed(address: SocketAddr) {
    until(|| TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_err());
}
#[test]
fn http_io_stop_and_timeout_through_rpc() {
    let mut p = Plugin::spawn();
    assert_eq!(
        p.request("initialize", json!({}))["name"],
        "background-terminals"
    );
    let (server, address) = start_http(&mut p);
    let mut stream = TcpStream::connect_timeout(&address, WAIT).unwrap();
    stream.set_read_timeout(Some(WAIT)).unwrap();
    stream
        .write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.0 200"), "{response}");
    let log = read_until(&mut p, &server, |v| {
        v["output"].as_str().unwrap().contains("GET / HTTP/1.0")
    });
    let cursor = log["cursor"].as_u64().unwrap();
    assert_eq!(
        p.call("terminal_read", json!({"id":server,"cursor":cursor}))["output"],
        ""
    );
    let list = p.call("terminal_list", json!({}));
    assert!(list["terminals"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v["id"] == server));
    assert_eq!(
        p.call("terminal_stop", json!({"id":server}))["status"],
        "stopped"
    );
    assert_closed(address);
    let term = p.call(
        "terminal_start",
        json!({"command":"read line; printf 'received:%s\\n' \"$line\""}),
    );
    let id = term["id"].as_str().unwrap();
    assert_eq!(
        p.call("terminal_write", json!({"id":id,"input":"hello\n"}))["written"],
        6
    );
    let output = read_until(&mut p, id, |v| v["status"] == "exited");
    assert!(output["output"]
        .as_str()
        .unwrap()
        .contains("received:hello"));
    assert_eq!(output["exit_code"], 0);
    let term = p.call(
        "terminal_start",
        json!({"command":"sleep 30", "timeout_ms":100}),
    );
    let timed_out = read_until(&mut p, term["id"].as_str().unwrap(), |v| {
        v["status"] != "running"
    });
    assert_eq!(timed_out["status"], "timed_out");
    assert_eq!(p.request("shutdown", json!({})), Value::Null);
    p.wait_exit(true);
}
#[test]
fn session_and_workspace_scope_is_enforced() {
    let mut p = Plugin::spawn();
    let term = p.call("terminal_start", json!({"command":"sleep 30"}));
    let id = term["id"].as_str().unwrap();
    for (session, workspace) in [("other", "/tmp"), ("integration", "/")] {
        let list = p.raw_call("terminal_list", json!({}), session, workspace);
        assert_eq!(list["ok"], true);
        assert!(list["meta"]["terminals"].as_array().unwrap().is_empty());
        for (name, args) in [
            ("terminal_read", json!({"id":id})),
            ("terminal_write", json!({"id":id,"input":"x"})),
            ("terminal_stop", json!({"id":id})),
        ] {
            let result = p.raw_call(name, args, session, workspace);
            assert_eq!(result["ok"], false, "{result}");
        }
    }
    assert_eq!(
        p.call("terminal_read", json!({"id":id}))["status"],
        "running"
    );
    assert_eq!(
        p.call("terminal_stop", json!({"id":id}))["status"],
        "stopped"
    );
}
#[test]
fn eof_shutdown_closes_running_http_descendant() {
    let mut p = Plugin::spawn();
    let (_, address) = start_http(&mut p);
    assert!(TcpStream::connect_timeout(&address, WAIT).is_ok());
    p.stdin.take();
    p.wait_exit(true);
    assert_closed(address);
}
#[test]
fn signals_close_running_http_descendant() {
    for signal in [libc::SIGTERM, libc::SIGINT] {
        let mut p = Plugin::spawn();
        let (_, address) = start_http(&mut p);
        assert!(TcpStream::connect_timeout(&address, WAIT).is_ok());
        assert_eq!(unsafe { libc::kill(p.child.id() as i32, signal) }, 0);
        p.wait_exit(false);
        assert_closed(address);
    }
}
