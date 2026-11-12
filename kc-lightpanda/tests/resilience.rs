//! Multi-connection failure/recovery tests using only local fake CDP sockets.

use kc_lightpanda::browser::{BrowserConfig, BrowserManager};
use kc_lightpanda::cdp::TransportError;
use serde_json::{json, Value};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};
use tungstenite::Message;

struct FakeCdpServer {
    endpoint: String,
    close_pages: Arc<Vec<AtomicBool>>,
    disconnected: mpsc::Receiver<usize>,
    acceptor: Option<std::thread::JoinHandle<()>>,
}

impl FakeCdpServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("ws://{}/", listener.local_addr().unwrap());
        let close_pages = Arc::new(vec![
            AtomicBool::new(false),
            AtomicBool::new(false),
            AtomicBool::new(false),
        ]);
        let (disconnected_sender, disconnected) = mpsc::channel();
        let close_for_server = Arc::clone(&close_pages);
        let acceptor = std::thread::spawn(move || {
            let mut workers = Vec::new();
            for index in 0..3 {
                let (tcp, _) = listener.accept().expect("fake CDP connection");
                tcp.set_read_timeout(Some(Duration::from_millis(50)))
                    .unwrap();
                let close = Arc::clone(&close_for_server);
                let disconnected = disconnected_sender.clone();
                workers.push(std::thread::spawn(move || {
                    serve_page(tcp, index, close, disconnected);
                }));
            }
            for worker in workers {
                worker.join().expect("fake CDP worker");
            }
        });
        Self {
            endpoint,
            close_pages,
            disconnected,
            acceptor: Some(acceptor),
        }
    }

    fn config(&self) -> BrowserConfig {
        BrowserConfig {
            web_socket_url: Some(self.endpoint.clone()),
            protocol_timeout_secs: 1,
            ..Default::default()
        }
    }

    fn close_page(&self, page_index: usize) {
        self.close_pages[page_index].store(true, Ordering::Release);
        assert_eq!(
            self.disconnected
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            page_index
        );
    }

    fn finish(mut self) {
        if let Some(acceptor) = self.acceptor.take() {
            acceptor.join().expect("fake CDP acceptor");
        }
    }
}

fn response(request: &Value, index: usize) -> Value {
    let method = request["method"].as_str().unwrap();
    let result = match method {
        "Target.createBrowserContext" => json!({"browserContextId": format!("BID-{index}")}),
        "Target.createTarget" => json!({"targetId": format!("FID-{index}")}),
        "Target.attachToTarget" => json!({"sessionId": format!("SID-{index}")}),
        "Runtime.evaluate" if request["params"]["expression"].as_str() == Some("undefined") => {
            json!({"result": {"type": "undefined"}})
        }
        "Runtime.evaluate"
            if request["params"]["expression"].as_str() == Some("document.title") =>
        {
            json!({"result": {"type": "string", "value": format!("Page {index}")}})
        }
        "Runtime.evaluate" => json!({"result": {"type": "object", "value": {
            "url": format!("http://fake.test/page-{index}"), "title": format!("Page {index}")
        }}}),
        _ => json!({}),
    };
    let mut response = json!({"id": request["id"], "result": result});
    if let Some(session_id) = request.get("sessionId") {
        response["sessionId"] = session_id.clone();
    }
    // When the second page finishes its setup, simulate a server-side loss of
    // only the first page's TCP connection. Ordinary page creation still works.
    if index == 1 && method == "Emulation.setDeviceMetricsOverride" {
        response["params"] = json!({"closePage": 0});
    }
    response
}

fn serve_page(
    tcp: TcpStream,
    index: usize,
    close_pages: Arc<Vec<AtomicBool>>,
    disconnected: mpsc::Sender<usize>,
) {
    let mut socket = tungstenite::accept(tcp).expect("fake CDP WebSocket handshake");
    loop {
        if close_pages[index].load(Ordering::Acquire) {
            break;
        }
        match socket.read() {
            Ok(Message::Text(text)) => {
                let request: Value = serde_json::from_str(&text).expect("fake CDP request JSON");
                let mut reply = response(&request, index);
                if let Some(close_index) = reply
                    .get("params")
                    .and_then(|p| p.get("closePage"))
                    .and_then(|v| v.as_u64())
                {
                    close_pages[close_index as usize].store(true, Ordering::Release);
                }
                if let Some(params) = reply.as_object_mut() {
                    params.remove("params");
                }
                socket
                    .send(Message::Text(reply.to_string().into()))
                    .unwrap();
            }
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            Err(tungstenite::Error::Io(err))
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => break,
            Err(_) => break,
        }
    }
    drop(socket);
    let _ = disconnected.send(index);
}

#[test]
fn one_disconnected_page_does_not_hide_healthy_pages_or_stop_recovery() {
    let server = FakeCdpServer::start();
    let mut browser = BrowserManager::new(server.config());
    let first = browser.create_page("about:blank", true).unwrap();
    let second = browser.create_page("about:blank", false).unwrap();
    assert_eq!((first.id, second.id), (1, 2));
    server.close_page(0);

    let pages = browser
        .page_targets()
        .expect("healthy pages remain listable");
    assert_eq!(pages.iter().map(|p| p.id).collect::<Vec<_>>(), vec![2]);
    assert_eq!(pages[0].title, "Page 1");
    assert_eq!(
        browser
            .evaluate(Some(second.id), "document.title", false)
            .unwrap(),
        json!("Page 1")
    );

    let third = browser.create_page("about:blank", true).unwrap();
    let recovered = browser.page_targets().unwrap();
    assert_eq!(
        recovered.iter().map(|p| p.id).collect::<Vec<_>>(),
        vec![2, 3]
    );
    assert_eq!(third.id, 3);

    // Console reads must surface a later disconnect, not pretend there were no
    // messages. This also checks the post-pump error path specifically.
    server.close_page(1);
    let started = Instant::now();
    let error = browser.take_console(Some(second.id)).unwrap_err();
    assert!(matches!(error, TransportError::Closed(_)), "{error}");
    assert!(started.elapsed() < Duration::from_secs(2));
    let after_second_close = browser.page_targets().unwrap();
    assert_eq!(
        after_second_close.iter().map(|p| p.id).collect::<Vec<_>>(),
        vec![3]
    );
    assert!(
        browser.take_console(Some(first.id)).is_err(),
        "explicit dead ids must not become a new page"
    );

    browser.shutdown();
    server.finish();
}
