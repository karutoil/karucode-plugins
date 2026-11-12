//! Bounded-transport regressions that need no Lightpanda installation.

use kc_lightpanda::cdp::{http_get, PageSocket, TransportError};
use serde_json::json;
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn assert_bounded<T>(start: Instant, outcome: &Result<T, TransportError>, host: &str) {
    assert!(outcome.is_err(), "{host} unexpectedly succeeded");
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "{host} exceeded its 250ms deadline plus scheduling grace: {:?}",
        start.elapsed()
    );
}

#[test]
fn websocket_upgrade_is_bounded_when_the_peer_never_answers_http() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (release_sender, release_receiver) = mpsc::channel::<()>();
    let server = std::thread::spawn(move || {
        let (_socket, _) = listener.accept().unwrap();
        let _ = release_receiver.recv_timeout(Duration::from_secs(3));
    });
    let start = Instant::now();
    let endpoint = format!("ws://{address}/");
    let result = PageSocket::connect(&endpoint, Duration::from_millis(250));
    assert_bounded(start, &result, "silent WebSocket upgrade");
    match result {
        Ok(_) => unreachable!("already asserted this connection failed"),
        Err(error) => assert!(error.to_string().contains("could not attach")),
    }
    release_sender.send(()).unwrap();
    server.join().unwrap();
}

#[test]
fn readiness_http_read_is_bounded_by_the_response_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (release_sender, release_receiver) = mpsc::channel::<()>();
    let server = std::thread::spawn(move || {
        let (_socket, _) = listener.accept().unwrap();
        let _ = release_receiver.recv_timeout(Duration::from_secs(3));
    });
    let start = Instant::now();
    let result = http_get(
        &address.ip().to_string(),
        address.port(),
        "/json/version",
        Duration::from_millis(250),
    );
    assert_bounded(start, &result, "silent HTTP readiness response");
    release_sender.send(()).unwrap();
    server.join().unwrap();
}

#[test]
fn a_nonreading_websocket_peer_cannot_block_a_large_write_forever() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (release_sender, release_receiver) = mpsc::channel::<()>();
    let server = std::thread::spawn(move || {
        let (socket, _) = listener.accept().unwrap();
        let _websocket = tungstenite::accept(socket).expect("test WebSocket handshake");
        // Keep the connection alive but deliberately do not drain any frames.
        let _ = release_receiver.recv_timeout(Duration::from_secs(3));
    });
    let mut socket =
        PageSocket::connect(&format!("ws://{address}/"), Duration::from_millis(250)).unwrap();
    let start = Instant::now();
    let result = socket.send(
        "Runtime.evaluate",
        json!({"expression": "x".repeat(16 * 1024 * 1024)}),
    );
    assert_bounded(start, &result, "non-reading WebSocket peer during a write");
    release_sender.send(()).unwrap();
    server.join().unwrap();
}
