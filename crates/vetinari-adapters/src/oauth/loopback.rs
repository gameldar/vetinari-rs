//! RFC 8252 loopback sign-in with PKCE (RFC 7636).
//!
//! For a provider that will not grant its scopes to a device-code client,
//! as Google will not for Calendar. Vetinari listens on a free port on
//! 127.0.0.1, opens the authorisation page in the browser, and waits for the
//! browser to come back with a code. The wait runs in the background so the
//! sign-in fits the same start-then-poll shape as a device-code flow: the
//! challenge's `device_code` is the request's `state`, and a poll reads
//! whatever the listener has seen for it.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use vetinari_core::application::ports::{
    DeviceCodeChallenge, DeviceCodePoll, PortError, PortResult,
};
use vetinari_core::domain::credentials::Provider;

use super::device_code::Exchange;

/// How long the browser has to come back.
const WAIT: std::time::Duration = std::time::Duration::from_secs(300);
/// The most of a request's head the listener reads.
const MAX_HEAD: usize = 8 * 1024;

/// One sign-in to begin: the provider's endpoints and the client to use.
pub(crate) struct Request<'a> {
    pub provider: Provider,
    pub authorization_url: &'a str,
    pub token_url: String,
    pub scopes: &'a [&'a str],
    pub extra_parameters: &'a [(&'a str, &'a str)],
    pub client_id: String,
    pub client_secret: Option<String>,
}

/// Loopback sign-ins in progress, keyed by state.
pub(crate) struct Loopback {
    exchange: Arc<Exchange>,
    outcomes: Arc<Mutex<HashMap<String, DeviceCodePoll>>>,
    open_browser: bool,
}

impl Loopback {
    pub(crate) fn new(exchange: Arc<Exchange>) -> Self {
        Self {
            exchange,
            outcomes: Arc::new(Mutex::new(HashMap::new())),
            open_browser: true,
        }
    }

    /// Leave the browser closed, for testing.
    pub(crate) const fn without_browser(mut self) -> Self {
        self.open_browser = false;
        self
    }

    /// Listen for the browser, then send it to the authorisation page.
    pub(crate) async fn start(&self, request: Request<'_>) -> PortResult<DeviceCodeChallenge> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|error| PortError::unavailable("oauth", error.to_string()))?;
        let port = listener
            .local_addr()
            .map_err(|error| PortError::unavailable("oauth", error.to_string()))?
            .port();
        let redirect_uri = format!("http://127.0.0.1:{port}/");

        let state = random_token(16)?;
        let verifier = random_token(32)?;
        let code_challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let scope = request.scopes.join(" ");

        let mut query = vec![
            ("client_id", request.client_id.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("response_type", "code"),
            ("scope", scope.as_str()),
            ("state", state.as_str()),
            ("code_challenge", code_challenge.as_str()),
            ("code_challenge_method", "S256"),
        ];
        query.extend_from_slice(request.extra_parameters);
        let page = reqwest::Url::parse_with_params(request.authorization_url, &query)
            .map_err(|error| PortError::protocol("oauth", error.to_string()))?
            .to_string();

        self.outcomes
            .lock()
            .expect("lock")
            .insert(state.clone(), DeviceCodePoll::Pending);

        let wait = Wait {
            exchange: self.exchange.clone(),
            provider: request.provider,
            state: state.clone(),
            verifier,
            redirect_uri,
            token_url: request.token_url,
            client_id: request.client_id,
            client_secret: request.client_secret,
        };
        let outcomes = self.outcomes.clone();
        let waited_for = state.clone();
        tokio::spawn(async move {
            let outcome = tokio::time::timeout(WAIT, wait.run(listener))
                .await
                .unwrap_or_else(|_| DeviceCodePoll::Rejected {
                    reason: "the browser did not come back in time".to_owned(),
                });
            if let Some(slot) = outcomes.lock().expect("lock").get_mut(&waited_for) {
                *slot = outcome;
            }
        });

        if self.open_browser {
            open_in_browser(&page);
        }

        Ok(DeviceCodeChallenge {
            user_code: None,
            verification_uri: page,
            device_code: state,
            interval_secs: 1,
            expires_at: self.exchange.now() + Duration::seconds(WAIT.as_secs().cast_signed()),
        })
    }

    /// What the listener has seen for the sign-in `state` began.
    pub(crate) fn poll(&self, state: &str) -> DeviceCodePoll {
        let mut outcomes = self.outcomes.lock().expect("lock");
        match outcomes.get(state) {
            None => DeviceCodePoll::Rejected {
                reason: "no sign-in is in progress".to_owned(),
            },
            Some(DeviceCodePoll::Pending) => DeviceCodePoll::Pending,
            Some(_) => outcomes.remove(state).unwrap_or(DeviceCodePoll::Pending),
        }
    }
}

/// The background half of one sign-in.
struct Wait {
    exchange: Arc<Exchange>,
    provider: Provider,
    state: String,
    verifier: String,
    redirect_uri: String,
    token_url: String,
    client_id: String,
    client_secret: Option<String>,
}

impl Wait {
    /// Answer the browser until it brings a code or an error for our state.
    async fn run(self, listener: TcpListener) -> DeviceCodePoll {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                continue;
            };
            let Some(query) = read_callback(&mut stream).await else {
                respond(&mut stream, 404, "Not found.").await;
                continue;
            };
            if query.get("state") != Some(&self.state) {
                respond(
                    &mut stream,
                    400,
                    "This sign-in is not the one Vetinari started.",
                )
                .await;
                continue;
            }
            if let Some(error) = query.get("error") {
                respond(
                    &mut stream,
                    200,
                    "Sign-in was declined. You can close this tab.",
                )
                .await;
                return DeviceCodePoll::Rejected {
                    reason: match error.as_str() {
                        "access_denied" => "the request was declined".to_owned(),
                        other => other.to_owned(),
                    },
                };
            }
            let Some(code) = query.get("code") else {
                respond(&mut stream, 400, "The reply carried no code.").await;
                continue;
            };

            let outcome = self.redeem(code).await;
            let page = match &outcome {
                DeviceCodePoll::Granted(_) => {
                    "Vetinari is signed in. You can close this tab.".to_owned()
                }
                DeviceCodePoll::Rejected { reason } => {
                    format!("Sign-in failed: {reason}. You can close this tab.")
                }
                DeviceCodePoll::Pending => "Still waiting.".to_owned(),
            };
            respond(&mut stream, 200, &page).await;
            return outcome;
        }
    }

    /// Exchange the authorisation code for a credential.
    async fn redeem(&self, code: &str) -> DeviceCodePoll {
        let mut form = vec![
            ("client_id", self.client_id.as_str()),
            ("code", code),
            ("code_verifier", self.verifier.as_str()),
            ("grant_type", "authorization_code"),
            ("redirect_uri", self.redirect_uri.as_str()),
        ];
        if let Some(secret) = &self.client_secret {
            form.push(("client_secret", secret.as_str()));
        }
        match self.exchange.poll(&self.token_url, &form).await {
            // A code is redeemed once; there is nothing to keep waiting for.
            Ok(DeviceCodePoll::Pending) => DeviceCodePoll::Rejected {
                reason: format!("{} did not issue a token", self.provider.name()),
            },
            Ok(outcome) => outcome,
            Err(error) => DeviceCodePoll::Rejected {
                reason: error.to_string(),
            },
        }
    }
}

/// Read a request's head and return the query of a `GET /`, or `None` for
/// anything else the browser asks for, such as a favicon.
async fn read_callback(stream: &mut TcpStream) -> Option<HashMap<String, String>> {
    let mut head = Vec::new();
    let mut buffer = [0_u8; 1024];
    while !head.windows(4).any(|window| window == b"\r\n\r\n") && head.len() < MAX_HEAD {
        let read = stream.read(&mut buffer).await.ok()?;
        if read == 0 {
            break;
        }
        head.extend_from_slice(&buffer[..read]);
    }

    let head = String::from_utf8_lossy(&head);
    let mut request_line = head.lines().next()?.split_whitespace();
    if request_line.next()? != "GET" {
        return None;
    }
    let url = reqwest::Url::parse(&format!("http://127.0.0.1{}", request_line.next()?)).ok()?;
    (url.path() == "/").then(|| url.query_pairs().into_owned().collect())
}

async fn respond(stream: &mut TcpStream, status: u16, message: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        _ => "Not Found",
    };
    let message = message
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let body =
        format!("<!doctype html><meta charset=\"utf-8\"><title>Vetinari</title><p>{message}</p>");
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// `bytes` random bytes, base64url-encoded.
fn random_token(bytes: usize) -> PortResult<String> {
    let mut buffer = vec![0_u8; bytes];
    getrandom::fill(&mut buffer)
        .map_err(|error| PortError::unavailable("oauth", error.to_string()))?;
    Ok(URL_SAFE_NO_PAD.encode(buffer))
}

/// Best effort: the challenge still names the page if no browser opens.
fn open_in_browser(page: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    if let Ok(mut child) = tokio::process::Command::new(opener)
        .arg(page)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        tokio::spawn(async move {
            let _ = child.wait().await;
        });
    }
}
