//! The daemon's second inbound adapter: the web page's HTTP and event
//! streams over the same board and backend the socket serves (design 10.3).
//!
//! It listens on loopback only. The page's code is public, so its assets
//! need nothing; everything under `/api` needs the token the daemon makes
//! when it starts, which only someone who can read the daemon's runtime
//! directory or its log has.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::convert::Infallible;
use std::num::NonZeroU32;
use std::sync::Arc;

use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::stream::{self, Stream, StreamExt};
use serde::Deserialize;
use tokio::sync::{broadcast, watch};
use vetinari_core::application::devflow::api::{ApiError, BoardChange};
use vetinari_core::application::devflow::worker::GateReply;
use vetinari_core::domain::devflow::compile::{WorkerQueues, preview};
use vetinari_core::domain::devflow::ids::{NodeId, NodeInstance, Rounds, RunId};
use vetinari_core::domain::devflow::lanes::IterationSelection;
use vetinari_core::domain::devflow::outcome::{GateAnswer, Line};
use vetinari_web_model as model;

use crate::hub::Hub;
use crate::socket::{Backend, run_detail, signed_in_change};
use crate::view::{entry, list_event, preview_drawing, run_view};

/// Where a daemon on `socket` writes its page's URL, token included.
#[must_use]
pub fn url_file(socket: &std::path::Path) -> std::path::PathBuf {
    socket.with_file_name("web.url")
}

/// The page's URL that the daemon on `socket` wrote when it started.
pub fn read_url(socket: &std::path::Path) -> std::io::Result<String> {
    Ok(std::fs::read_to_string(url_file(socket))?.trim().to_owned())
}

/// Serve the page on `addr` until the task is dropped, and write its URL
/// beside `socket` where only this user can read it. An address that
/// cannot be bound leaves the daemon without a page rather than without a
/// socket.
pub async fn start(
    addr: LoopbackAddr,
    socket: &std::path::Path,
    web: Web,
) -> Option<tokio::task::JoinHandle<()>> {
    let listener = match tokio::net::TcpListener::bind(addr.get()).await {
        Ok(listener) => listener,
        Err(error) => {
            tracing::warn!(%error, addr = %addr.get(), "cannot serve the web page");
            return None;
        }
    };
    let bound = listener.local_addr().ok()?;
    let url = format!("http://{bound}/?token={}", web.token.as_str());
    if let Err(error) = write_private(&url_file(socket), &url) {
        tracing::warn!(%error, "cannot write the web page's URL");
    }
    tracing::info!(addr = %bound, "web page served; `vetinarid web` prints its URL");
    let app = router(Arc::new(web));
    Some(tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app).await {
            tracing::warn!(%error, "the web page stopped");
        }
    }))
}

fn write_private(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let _ = std::fs::remove_file(path);
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?
        .write_all(format!("{text}\n").as_bytes())
}

/// The page as built by `trunk build` in `crates/vetinari-web`: read from
/// disk in a debug build, embedded in a release build.
#[derive(rust_embed::RustEmbed)]
#[folder = "../vetinari-web/dist/"]
#[allow_missing = true]
struct Built;

/// The built page, or a note saying how to build it.
pub struct BuiltPage;

const UNBUILT: &str = "<!doctype html><meta charset=utf-8><title>vetinari</title>\
<p>The page is not built. Run <code>trunk build --release</code> in \
<code>crates/vetinari-web</code>, then rebuild vetinarid.</p>";

impl Assets for BuiltPage {
    fn get(&self, path: &str) -> Option<Asset> {
        match Built::get(path) {
            Some(file) => Some(Asset {
                content_type: Cow::Owned(file.metadata.mimetype().to_owned()),
                bytes: file.data,
            }),
            None if path == "index.html" => Some(Asset {
                bytes: Cow::Borrowed(UNBUILT.as_bytes()),
                content_type: Cow::Borrowed("text/html; charset=utf-8"),
            }),
            None => None,
        }
    }
}

/// An address the page may be served on: loopback only, so nothing off
/// this machine can reach it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoopbackAddr(std::net::SocketAddr);

impl LoopbackAddr {
    /// Where the page is served unless told otherwise.
    pub const DEFAULT: &'static str = "127.0.0.1:7878";

    /// Accept `host:port` on a loopback address.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let addr: std::net::SocketAddr = raw.parse().map_err(|_| {
            format!(
                "{raw} is not an address and port, such as {}",
                Self::DEFAULT
            )
        })?;
        if addr.ip().is_loopback() {
            Ok(Self(addr))
        } else {
            Err(format!(
                "{raw} is not a loopback address: the page is served on this machine only"
            ))
        }
    }

    /// The address.
    #[must_use]
    pub const fn get(self) -> std::net::SocketAddr {
        self.0
    }
}

/// A file of the page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    /// Its bytes.
    pub bytes: Cow<'static, [u8]>,
    /// Its media type.
    pub content_type: Cow<'static, str>,
}

/// Where the page's files come from.
pub trait Assets: Send + Sync {
    /// The file at `path`, relative to the page's root, if there is one.
    fn get(&self, path: &str) -> Option<Asset>;
}

/// The secret every `/api` request carries, as a bearer token or as
/// `?token=` where a browser cannot set a header.
#[derive(Clone, PartialEq, Eq)]
pub struct WebToken(String);

impl WebToken {
    /// A fresh token from the system's randomness.
    pub fn generate() -> std::io::Result<Self> {
        let mut bytes = [0_u8; 24];
        getrandom::fill(&mut bytes).map_err(|error| std::io::Error::other(error.to_string()))?;
        Ok(Self(
            bytes.iter().map(|byte| format!("{byte:02x}")).collect(),
        ))
    }

    /// The token as it goes in a URL.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for WebToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WebToken(..)")
    }
}

/// What the web adapter serves from.
pub struct Web {
    /// The run board.
    pub hub: Arc<Hub>,
    /// Gates, cancels, dismissals and outputs.
    pub backend: Arc<Backend>,
    /// This machine's queues, for the preview.
    pub queues: WorkerQueues,
    /// The page's files.
    pub assets: Arc<dyn Assets>,
    /// The `/api` secret.
    pub token: WebToken,
}

/// The routes, ready to serve.
pub fn router(web: Arc<Web>) -> Router {
    let api = Router::new()
        .route("/preview", get(preview_template))
        .route("/runs", get(watch_list))
        .route("/runs/{run}", get(watch_run))
        .route("/runs/{run}/gates/{instance}", post(answer_gate))
        .route("/runs/{run}/cancel", post(cancel_run))
        .route("/runs/{run}/dismiss", post(dismiss_run))
        .route_layer(middleware::from_fn_with_state(web.clone(), authorised));
    Router::new()
        .nest("/api", api)
        .fallback(get(asset))
        .with_state(web)
}

async fn authorised(
    State(web): State<Arc<Web>>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Response {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let query = request.uri().query().and_then(|query| {
        query
            .split('&')
            .find_map(|pair| pair.strip_prefix("token="))
    });
    if bearer
        .or(query)
        .is_some_and(|given| same(given, web.token.as_str()))
    {
        next.run(request).await
    } else {
        problem(StatusCode::UNAUTHORIZED, "this needs the page's token")
    }
}

/// Whether two secrets are equal, taking as long whichever byte differs.
fn same(given: &str, token: &str) -> bool {
    given.len() == token.len()
        && given
            .bytes()
            .zip(token.bytes())
            .fold(0, |differ, (a, b)| differ | (a ^ b))
            == 0
}

fn problem(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(model::Problem {
            message: message.into(),
        }),
    )
        .into_response()
}

fn refused(error: &ApiError) -> Response {
    let status = match error {
        ApiError::Invalid(_) => StatusCode::BAD_REQUEST,
        ApiError::NotFound(_) => StatusCode::NOT_FOUND,
        ApiError::Conflict(_) => StatusCode::CONFLICT,
        ApiError::Unavailable(_) | ApiError::Unauthenticated(_) => StatusCode::SERVICE_UNAVAILABLE,
        ApiError::Protocol(_) => StatusCode::BAD_GATEWAY,
    };
    problem(status, error.to_string())
}

#[derive(Deserialize)]
struct PreviewParams {
    intake: Option<u32>,
    review: Option<u32>,
}

async fn preview_template(
    State(web): State<Arc<Web>>,
    Query(params): Query<PreviewParams>,
) -> Response {
    let rounds = |count: Option<u32>| count.map_or(Ok(Rounds::DEFAULT), Rounds::new);
    match (rounds(params.intake), rounds(params.review)) {
        (Ok(intake), Ok(review)) => {
            let (nodes, edges) = preview(web.backend.template, intake, review, &web.queues);
            Json(preview_drawing(&nodes, &edges)).into_response()
        }
        (Err(error), _) | (_, Err(error)) => problem(StatusCode::BAD_REQUEST, error.to_string()),
    }
}

fn sse<T: serde::Serialize>(
    events: impl Stream<Item = T> + Send + 'static,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    Sse::new(
        events.map(|event| {
            Ok(Event::default().data(serde_json::to_string(&event).unwrap_or_default()))
        }),
    )
    .keep_alive(KeepAlive::default())
}

/// What the run list has still to send, and where the rest comes from.
struct ListFeed {
    pending: VecDeque<model::ListEvent>,
    changes: broadcast::Receiver<BoardChange>,
    sign_in: Option<watch::Receiver<bool>>,
    hub: Arc<Hub>,
}

async fn watch_list(State(web): State<Arc<Web>>) -> impl IntoResponse {
    let (snapshot, changes) = web.hub.watch();
    let mut sign_in = web
        .backend
        .sign_in
        .as_ref()
        .map(|sign_in| sign_in.changes());
    let mut pending = VecDeque::from([model::ListEvent::Snapshot {
        runs: snapshot.iter().map(entry).collect(),
    }]);
    if let Some(needed) = sign_in.as_mut() {
        pending.push_back(model::ListEvent::SignIn {
            needed: *needed.borrow_and_update(),
        });
    }
    let feed = ListFeed {
        pending,
        changes,
        sign_in,
        hub: web.hub.clone(),
    };
    sse(stream::unfold(feed, |mut feed| async move {
        if let Some(event) = feed.pending.pop_front() {
            return Some((event, feed));
        }
        let event = tokio::select! {
            change = feed.changes.recv() => match change {
                Ok(change) => list_event(&change),
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let (snapshot, fresh) = feed.hub.watch();
                    feed.changes = fresh;
                    model::ListEvent::Snapshot { runs: snapshot.iter().map(entry).collect() }
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            },
            Some(needed) = signed_in_change(feed.sign_in.as_mut()) => {
                model::ListEvent::SignIn { needed }
            }
        };
        Some((event, feed))
    }))
}

#[derive(Deserialize)]
struct RunParams {
    rounds: Option<String>,
}

/// `rework:1,intake:2`: the round each named loop is looked at on.
fn selection(rounds: Option<&str>) -> Result<IterationSelection, String> {
    rounds
        .into_iter()
        .flat_map(|rounds| rounds.split(','))
        .filter(|pair| !pair.is_empty())
        .try_fold(IterationSelection::default(), |selection, pair| {
            let (node, round) = pair
                .split_once(':')
                .ok_or_else(|| format!("{pair} is not loop:round"))?;
            let node = NodeId::parse(node).map_err(|error| error.to_string())?;
            let round = round
                .parse::<NonZeroU32>()
                .map_err(|_| format!("{round} is not a round"))?;
            Ok(selection.with(node, round))
        })
}

/// Where one run's stream is.
struct RunFeed {
    web: Arc<Web>,
    run: RunId,
    selection: IterationSelection,
    touches: broadcast::Receiver<RunId>,
    first: bool,
}

async fn watch_run(
    State(web): State<Arc<Web>>,
    Path(run): Path<String>,
    Query(params): Query<RunParams>,
) -> Response {
    let Ok(run) = RunId::parse(&run) else {
        return problem(StatusCode::BAD_REQUEST, "that is not a run id");
    };
    let selection = match selection(params.rounds.as_deref()) {
        Ok(selection) => selection,
        Err(message) => return problem(StatusCode::BAD_REQUEST, message),
    };
    if web.hub.run(&run).is_none() {
        return problem(StatusCode::NOT_FOUND, format!("run {run}"));
    }
    let feed = RunFeed {
        touches: web.hub.touches(),
        web,
        run,
        selection,
        first: true,
    };
    sse(stream::unfold(feed, |mut feed| async move {
        if !feed.first {
            loop {
                match feed.touches.recv().await {
                    Ok(run) if run == feed.run => break,
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => break,
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        }
        feed.first = false;
        let (detail, graph) =
            run_detail(&feed.web.hub, &feed.web.backend, &feed.run, &feed.selection).await?;
        Some((run_view(&detail, &graph), feed))
    }))
    .into_response()
}

async fn answer_gate(
    State(web): State<Arc<Web>>,
    Path((run, instance)): Path<(String, String)>,
    Json(answer): Json<model::GateAnswer>,
) -> Response {
    let (Ok(run), Ok(instance)) = (RunId::parse(&run), NodeInstance::parse(&instance)) else {
        return problem(StatusCode::BAD_REQUEST, "that is not a run and a gate");
    };
    let answer = match answer {
        model::GateAnswer::Recheck { note } => {
            match note
                .filter(|note| !note.trim().is_empty())
                .map(|note| Line::parse(&note))
                .transpose()
            {
                Ok(note) => GateAnswer::Recheck { note },
                Err(error) => return problem(StatusCode::BAD_REQUEST, format!("note: {error}")),
            }
        }
        model::GateAnswer::OpenAnyway => GateAnswer::OpenAnyway,
    };
    match web.backend.gates.answer(&run, &instance, answer).await {
        Ok(reply) => Json(model::GateReply {
            sent: matches!(reply, GateReply::Sent),
            queued: matches!(reply, GateReply::Queued),
        })
        .into_response(),
        Err(error) => refused(&error),
    }
}

async fn cancel_run(State(web): State<Arc<Web>>, Path(run): Path<String>) -> Response {
    let Ok(run) = RunId::parse(&run) else {
        return problem(StatusCode::BAD_REQUEST, "that is not a run id");
    };
    match web.backend.runs.cancel(&run).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => refused(&error.into()),
    }
}

async fn dismiss_run(
    State(web): State<Arc<Web>>,
    Path(run): Path<String>,
    Json(dismiss): Json<model::Dismiss>,
) -> Response {
    let Ok(run) = RunId::parse(&run) else {
        return problem(StatusCode::BAD_REQUEST, "that is not a run id");
    };
    match web
        .backend
        .launches
        .dismiss(&run, dismiss.remove_worktree)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => refused(&error),
    }
}

async fn asset(State(web): State<Arc<Web>>, request: Request) -> Response {
    let path = match request.uri().path().trim_start_matches('/') {
        "" => "index.html",
        path => path,
    };
    match web.assets.get(path) {
        Some(asset) => (
            [(header::CONTENT_TYPE, asset.content_type.into_owned())],
            asset.bytes.into_owned(),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::num::NonZeroU32;

    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use axum::response::Response;
    use futures::StreamExt;
    use serde_json::{Value, json};
    use tower::ServiceExt;
    use vetinari_core::application::devflow::api::{RunItem, RunLabel};
    use vetinari_core::domain::devflow::compile::preview;
    use vetinari_core::domain::devflow::ids::NodeInstance;
    use vetinari_core::domain::devflow::ids::{Rounds, RunId, WorkerName};
    use vetinari_core::domain::devflow::outcome::{GateAnswer, Line};
    use vetinari_core::domain::devflow::run_view::{
        InstanceState, InstanceStatus, RunGraph, RunStatus,
    };
    use vetinari_core::domain::devflow::summary::RunSummary;
    use vetinari_core::domain::devflow::template::TemplateKind;
    use vetinari_core::domain::text::NonEmptyText;
    use vetinari_core::testing::FakeRunGateway;

    use super::*;
    use crate::fakes::{RecordingGates, RecordingLaunches};
    use crate::steps::LocalBoard;

    const TOKEN: &str = "s3cret";

    struct Page(BTreeMap<&'static str, &'static str>);

    impl Assets for Page {
        fn get(&self, path: &str) -> Option<Asset> {
            self.0.get(path).map(|text| Asset {
                bytes: Cow::Borrowed(text.as_bytes()),
                content_type: Cow::Borrowed("text/html; charset=utf-8"),
            })
        }
    }

    struct Fixture {
        app: Router,
        hub: Arc<Hub>,
        runs: Arc<FakeRunGateway>,
        gates: Arc<RecordingGates>,
        launches: Arc<RecordingLaunches>,
    }

    fn fixture() -> Fixture {
        let hub = Arc::new(Hub::default());
        let runs = Arc::new(FakeRunGateway::default());
        let gates = Arc::new(RecordingGates::default());
        let launches = Arc::new(RecordingLaunches::default());
        let backend = Arc::new(Backend {
            runs: runs.clone(),
            gates: gates.clone(),
            launches: launches.clone(),
            local: LocalBoard::new().0,
            template: TemplateKind::FeatureFromTicket,
            sign_in: None,
        });
        let app = router(Arc::new(Web {
            hub: hub.clone(),
            backend,
            queues: queues(),
            assets: Arc::new(Page(BTreeMap::from([("index.html", "<h1>vetinari</h1>")]))),
            token: WebToken(TOKEN.to_owned()),
        }));
        Fixture {
            app,
            hub,
            runs,
            gates,
            launches,
        }
    }

    fn queues() -> WorkerQueues {
        WorkerQueues::for_worker(&WorkerName::parse("hex").expect("worker"))
    }

    fn get(uri: &str) -> Request<Body> {
        Request::get(uri).body(Body::empty()).expect("request")
    }

    fn post(uri: &str, body: &Value) -> Request<Body> {
        Request::post(uri)
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .expect("request")
    }

    async fn call(app: &Router, request: Request<Body>) -> Response {
        app.clone().oneshot(request).await.expect("response")
    }

    async fn json_of(response: Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        serde_json::from_slice(&bytes).expect("json")
    }

    /// The `data:` of each event a stream sends, as JSON, until `count`.
    async fn events(response: Response, count: usize) -> Vec<Value> {
        let mut stream = response.into_body().into_data_stream();
        let mut text = String::new();
        let mut found = Vec::new();
        while found.len() < count {
            let chunk = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
                .await
                .expect("an event in time")
                .expect("the stream is open")
                .expect("a chunk");
            text.push_str(std::str::from_utf8(&chunk).expect("utf-8"));
            while let Some(end) = text.find("\n\n") {
                let event: String = text.drain(..end + 2).collect();
                if let Some(data) = event.lines().find_map(|line| line.strip_prefix("data: ")) {
                    found.push(serde_json::from_str(data).expect("event json"));
                }
            }
        }
        found
    }

    fn run() -> RunId {
        RunId::parse("r1").expect("run")
    }

    fn item() -> RunItem {
        RunItem {
            summary: RunSummary {
                run: run(),
                status: RunStatus::Active,
                phases: Vec::new(),
                needs_you: None,
            },
            label: Some(RunLabel {
                ticket: NonEmptyText::parse("ENG-412").expect("key"),
                title: NonEmptyText::parse("Rate-limit retries").expect("title"),
            }),
            started_at: None,
        }
    }

    fn graph(instances: Vec<InstanceState>) -> RunGraph {
        let (nodes, edges) = preview(
            TemplateKind::FeatureFromTicket,
            Rounds::DEFAULT,
            Rounds::DEFAULT,
            &queues(),
        );
        RunGraph {
            run: run(),
            status: RunStatus::Active,
            seq: 1,
            nodes,
            edges,
            instances,
        }
    }

    fn looping(iteration: u32) -> InstanceState {
        InstanceState {
            instance: NodeInstance::parse("rework").expect("instance"),
            status: InstanceStatus::Looping {
                iteration: NonZeroU32::new(iteration).expect("non-zero"),
            },
            attempt: None,
            first_ready_at: None,
            finished_at: None,
        }
    }

    #[tokio::test]
    async fn the_page_is_served_without_the_token_and_a_missing_file_is_not_found() {
        let fixture = fixture();

        let page = call(&fixture.app, get("/")).await;
        assert_eq!(page.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(page.into_body(), usize::MAX)
            .await
            .expect("body");
        assert_eq!(&bytes[..], b"<h1>vetinari</h1>");

        assert_eq!(
            call(&fixture.app, get("/missing.js")).await.status(),
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn the_api_refuses_a_request_without_the_token_or_with_another() {
        let fixture = fixture();

        for uri in [
            "/api/preview",
            "/api/preview?token=guess",
            "/api/runs",
            "/api/runs/r1",
        ] {
            assert_eq!(
                call(&fixture.app, get(uri)).await.status(),
                StatusCode::UNAUTHORIZED,
                "{uri}"
            );
        }
        let wrong = Request::get("/api/preview")
            .header(header::AUTHORIZATION, "Bearer guess")
            .body(Body::empty())
            .expect("request");
        assert_eq!(
            call(&fixture.app, wrong).await.status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn the_preview_draws_the_template_with_the_rounds_asked_for() {
        let fixture = fixture();

        let response = call(
            &fixture.app,
            get(&format!("/api/preview?token={TOKEN}&review=5")),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let drawing = json_of(response).await;
        assert_eq!(drawing["nodes"].as_array().map(Vec::len), Some(8));
        let rework = drawing["nodes"]
            .as_array()
            .and_then(|nodes| nodes.iter().find(|node| node["node"] == "rework"))
            .expect("rework");
        assert_eq!(rework["kind"]["bound"], json!(5));
        assert_eq!(fixture.runs.cancelled(), Vec::<String>::new());
    }

    #[tokio::test]
    async fn a_preview_with_rounds_out_of_range_is_refused() {
        let fixture = fixture();

        let response = call(
            &fixture.app,
            get(&format!("/api/preview?token={TOKEN}&review=0")),
        )
        .await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn the_run_list_streams_a_snapshot_then_each_change() {
        let fixture = fixture();
        fixture.hub.put(item());

        let response = call(&fixture.app, get(&format!("/api/runs?token={TOKEN}"))).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/event-stream"
        );
        let hub = fixture.hub.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            hub.remove(&run());
        });

        let seen = events(response, 2).await;
        assert_eq!(seen[0]["type"], "snapshot");
        assert_eq!(seen[0]["runs"][0]["ticket"], "ENG-412");
        assert_eq!(seen[1], json!({ "type": "removed", "run": "r1" }));
    }

    #[tokio::test]
    async fn a_run_streams_its_view_and_again_when_its_graph_changes() {
        let fixture = fixture();
        fixture.hub.put(item());
        fixture.hub.put_graph(graph(vec![looping(1)]));

        let response = call(&fixture.app, get(&format!("/api/runs/r1?token={TOKEN}"))).await;
        assert_eq!(response.status(), StatusCode::OK);
        let hub = fixture.hub.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let mut next = graph(vec![looping(2)]);
            next.seq = 2;
            hub.put_graph(next);
        });

        let seen = events(response, 2).await;
        let round = |view: &Value| {
            view["drawing"]["nodes"]
                .as_array()
                .and_then(|nodes| nodes.iter().find(|node| node["node"] == "rework"))
                .map(|rework| rework["kind"]["reached"].clone())
        };
        assert_eq!(seen[0]["entry"]["run"], "r1");
        assert_eq!(round(&seen[0]), Some(json!(1)));
        assert_eq!(round(&seen[1]), Some(json!(2)));
    }

    #[tokio::test]
    async fn a_run_is_drawn_at_the_rounds_asked_for() {
        let fixture = fixture();
        fixture.hub.put(item());
        fixture.hub.put_graph(graph(vec![looping(2)]));

        let response = call(
            &fixture.app,
            get(&format!("/api/runs/r1?token={TOKEN}&rounds=rework:1")),
        )
        .await;

        let seen = events(response, 1).await;
        let rework = seen[0]["drawing"]["nodes"]
            .as_array()
            .and_then(|nodes| nodes.iter().find(|node| node["node"] == "rework"))
            .cloned()
            .expect("rework");
        assert_eq!(rework["kind"]["selected"], json!(1));
    }

    #[tokio::test]
    async fn a_run_the_daemon_does_not_have_is_not_found_and_bad_rounds_are_refused() {
        let fixture = fixture();
        fixture.hub.put(item());
        fixture.hub.put_graph(graph(Vec::new()));

        assert_eq!(
            call(&fixture.app, get(&format!("/api/runs/gone?token={TOKEN}")))
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(
                &fixture.app,
                get(&format!("/api/runs/r1?token={TOKEN}&rounds=rework"))
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn a_gate_answer_goes_to_the_gates_with_its_note() {
        let fixture = fixture();

        let response = call(
            &fixture.app,
            post(
                "/api/runs/r1/gates/ask-info@1",
                &json!({ "kind": "recheck", "note": "50 a page" }),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            json_of(response).await,
            json!({ "sent": true, "queued": false })
        );
        assert_eq!(
            fixture.gates.0.lock().expect("lock").clone(),
            [(
                "r1".to_owned(),
                "ask-info@1".to_owned(),
                GateAnswer::Recheck {
                    note: Some(Line::parse("50 a page").expect("line"))
                }
            )]
        );
    }

    #[tokio::test]
    async fn a_cancel_goes_to_the_gateway_and_a_dismissal_to_the_launcher() {
        let fixture = fixture();

        let cancelled = call(&fixture.app, post("/api/runs/r1/cancel", &json!({}))).await;
        let dismissed = call(
            &fixture.app,
            post("/api/runs/r1/dismiss", &json!({ "remove_worktree": true })),
        )
        .await;

        assert_eq!(cancelled.status(), StatusCode::NO_CONTENT);
        assert_eq!(dismissed.status(), StatusCode::NO_CONTENT);
        assert_eq!(fixture.runs.cancelled(), ["r1"]);
        assert_eq!(
            fixture.launches.dismissed.lock().expect("lock").clone(),
            [("r1".to_owned(), true)]
        );
    }

    #[test]
    fn only_a_loopback_address_is_accepted() {
        assert_eq!(
            LoopbackAddr::parse("127.0.0.1:7878").map(LoopbackAddr::get),
            Ok("127.0.0.1:7878".parse().expect("addr"))
        );
        assert!(LoopbackAddr::parse("[::1]:0").is_ok());
        assert!(LoopbackAddr::parse(LoopbackAddr::DEFAULT).is_ok());
        for raw in [
            "0.0.0.0:7878",
            "192.168.1.20:80",
            "[::]:7878",
            "localhost",
            "7878",
        ] {
            assert!(LoopbackAddr::parse(raw).is_err(), "{raw}");
        }
    }

    #[test]
    fn a_generated_token_is_long_hex_and_never_printed_by_debug() {
        let token = WebToken::generate().expect("token");

        assert_eq!(token.as_str().len(), 48);
        assert!(token.as_str().chars().all(|ch| ch.is_ascii_hexdigit()));
        assert_ne!(WebToken::generate().expect("token"), token);
        assert_eq!(format!("{token:?}"), "WebToken(..)");
    }
}
