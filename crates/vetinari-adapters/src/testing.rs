//! A minimal HTTP server for testing the API-backed adapters.
//!
//! Enough HTTP/1.1 to let a real `reqwest` client talk to it: one response per
//! queued reply, every request recorded for inspection.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A request the server received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedRequest {
    /// The method.
    pub method: String,
    /// The path, including any query string.
    pub target: String,
    /// The headers, with lowercase names.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: String,
}

impl RecordedRequest {
    /// One header's value.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// The body as JSON.
    #[must_use]
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

/// A server that answers a fixed list of replies.
pub struct MockServer {
    /// The base URL to point an adapter at.
    pub base_url: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

/// One reply the server gives.
#[derive(Debug, Clone)]
pub enum Reply {
    /// A status and a JSON body with its length.
    Json(u16, String),
    /// A `200` event stream with no length: each chunk is written and
    /// flushed in turn, then the connection is closed.
    Stream(Vec<String>),
}

impl MockServer {
    /// Start a server that answers each queued `(status, body)` in turn.
    pub async fn start(replies: Vec<(u16, String)>) -> Self {
        Self::replying(
            replies
                .into_iter()
                .map(|(status, body)| Reply::Json(status, body))
                .collect(),
        )
        .await
    }

    /// Start a server that gives each queued reply in turn.
    pub async fn replying(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();

        tokio::spawn(async move {
            for reply in replies {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                if let Some(request) = read_request(&mut stream).await {
                    recorded.lock().expect("lock").push(request);
                }
                match reply {
                    Reply::Json(status, body) => {
                        let response = format!(
                            "HTTP/1.1 {status} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                            reason(status),
                            body.len()
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    }
                    Reply::Stream(chunks) => {
                        let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\nconnection: close\r\n\r\n";
                        let _ = stream.write_all(head.as_bytes()).await;
                        for chunk in chunks {
                            let _ = stream.flush().await;
                            let _ = stream.write_all(chunk.as_bytes()).await;
                        }
                    }
                }
                let _ = stream.flush().await;
                let _ = stream.shutdown().await;
            }
        });

        Self {
            base_url: format!("http://127.0.0.1:{port}"),
            requests,
        }
    }

    /// Start a server that answers one JSON body with 200.
    pub async fn with_json(body: &str) -> Self {
        Self::start(vec![(200, body.to_owned())]).await
    }

    /// The requests received so far.
    #[must_use]
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().expect("lock").clone()
    }

    /// The first request received.
    ///
    /// # Panics
    ///
    /// Panics if nothing has been received.
    #[must_use]
    pub fn request(&self) -> RecordedRequest {
        self.requests()
            .into_iter()
            .next()
            .expect("the server received no request")
    }
}

const fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

async fn read_request(stream: &mut tokio::net::TcpStream) -> Option<RecordedRequest> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 1024];
    let header_end = loop {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(index) = find_header_end(&buffer) {
            break index;
        }
    };

    let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    let mut lines = head.lines();
    let mut request_line = lines.next()?.split_whitespace();
    let method = request_line.next()?.to_owned();
    let target = request_line.next()?.to_owned();

    let mut headers = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_lowercase(), value.trim().to_owned()));
        }
    }

    let length: usize = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0);

    let mut body = buffer[header_end + 4..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }

    Some(RecordedRequest {
        method,
        target,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}
