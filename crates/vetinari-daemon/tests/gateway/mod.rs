//! A gateway played by a local HTTP server, routed by method and path so
//! the daemon's concurrent requests (the event stream, two claim loops,
//! heartbeats, graph fetches) each get their own answer.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

/// A request the gateway received.
#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub target: String,
    pub body: String,
}

impl Request {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

/// The gateway: one task to hand out on the agent queue, nothing else.
pub struct Gateway {
    pub base_url: String,
    completed: Mutex<Option<mpsc::UnboundedReceiver<Request>>>,
}

struct State {
    refuse: bool,
    claim: Mutex<Option<Value>>,
    completed: mpsc::UnboundedSender<Request>,
}

impl Gateway {
    pub async fn start(claim: Value) -> Self {
        Self::serving(Some(claim), false).await
    }

    /// A gateway that refuses every token it is sent.
    pub async fn refusing() -> Self {
        Self::serving(None, true).await
    }

    async fn serving(claim: Option<Value>, refuse: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let (completed, receiver) = mpsc::unbounded_channel();
        let state = Arc::new(State {
            refuse,
            claim: Mutex::new(claim),
            completed,
        });
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(answer(stream, state.clone()));
            }
        });
        Self {
            base_url: format!("http://127.0.0.1:{port}"),
            completed: Mutex::new(Some(receiver)),
        }
    }

    /// The first completion the gateway receives.
    pub async fn completed(&self) -> Request {
        let mut receiver = self
            .completed
            .lock()
            .expect("lock")
            .take()
            .expect("asked once");
        receiver.recv().await.expect("a completion")
    }
}

async fn answer(mut stream: TcpStream, state: Arc<State>) {
    let Some(request) = read(&mut stream).await else {
        return;
    };
    let path = request
        .target
        .split('?')
        .next()
        .unwrap_or_default()
        .to_owned();
    if state.refuse {
        reply(
            &mut stream,
            401,
            r#"{"error":{"code":"unauthenticated","message":"bad token"}}"#,
        )
        .await;
        return;
    }
    match (request.method.as_str(), path.as_str()) {
        ("GET", "/v1/runs/events") => {
            let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\n\r\n: connected\n\n";
            let _ = stream.write_all(head.as_bytes()).await;
            let _ = stream.flush().await;
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
        ("GET", "/v1/workflows") => reply(&mut stream, 200, "[]").await,
        ("POST", "/v1/tasks/claim") => {
            let agent = request.json()["queue"] == "vetinari-hex-agent";
            let claim = if agent {
                state.claim.lock().expect("lock").take()
            } else {
                None
            };
            match claim {
                Some(claim) => reply(&mut stream, 200, &claim.to_string()).await,
                None => {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    reply(&mut stream, 204, "").await;
                }
            }
        }
        ("POST", path) if path.ends_with("/heartbeat") => {
            reply(&mut stream, 200, r#"{"expires_at":"2999-01-01T00:00:00Z"}"#).await;
        }
        ("POST", path) if path.ends_with("/complete") => {
            let _ = state.completed.send(request.clone());
            reply(&mut stream, 204, "").await;
        }
        _ => {
            reply(
                &mut stream,
                404,
                r#"{"error":{"code":"not_found","message":"no such thing"}}"#,
            )
            .await;
        }
    }
}

async fn reply(stream: &mut TcpStream, status: u16, body: &str) {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        401 => "Unauthorized",
        _ => "Not Found",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
    let _ = stream.shutdown().await;
}

async fn read(stream: &mut TcpStream) -> Option<Request> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    let head_end = loop {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.lines();
    let mut first = lines.next()?.split_whitespace();
    let method = first.next()?.to_owned();
    let target = first.next()?.to_owned();
    let length = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while buffer.len() < head_end + length {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let body = String::from_utf8_lossy(&buffer[head_end..]).into_owned();
    Some(Request {
        method,
        target,
        body,
    })
}
