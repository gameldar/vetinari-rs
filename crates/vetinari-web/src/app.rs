//! The components: they render what `logic` decides and wire it to the
//! daemon's API.

use std::rc::Rc;

use futures::StreamExt;
use gloo_net::eventsource::futures::EventSource;
use gloo_net::http::Request;
use serde::de::DeserializeOwned;
use vetinari_web_model::{
    Dismiss, Drawing, GateAnswer, ListEvent, NeedsYou, NodeKind, Phase, RunStatus, RunView,
    ViewEdge, ViewNode,
};
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use web_sys::{HtmlInputElement, HtmlTextAreaElement};
use yew::prelude::*;

use crate::logic::{self, GateAction, Rounds, RunList};

/// Where the page is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Route {
    List,
    Run(String),
    Preview,
}

impl Route {
    fn from_hash(hash: &str) -> Self {
        let hash = hash.trim_start_matches('#');
        if let Some(run) = hash.strip_prefix("/run/").filter(|run| !run.is_empty()) {
            Self::Run(run.to_owned())
        } else if hash == "/preview" {
            Self::Preview
        } else {
            Self::List
        }
    }

    fn hash(&self) -> String {
        match self {
            Self::List => "#/".to_owned(),
            Self::Run(run) => format!("#/run/{run}"),
            Self::Preview => "#/preview".to_owned(),
        }
    }
}

fn window() -> web_sys::Window {
    web_sys::window().expect("a browser window")
}

/// The page.
#[component]
pub fn App() -> Html {
    let token = use_memo((), |()| {
        logic::token_in(&window().location().search().unwrap_or_default())
    });
    let route = use_state(|| Route::from_hash(&window().location().hash().unwrap_or_default()));
    {
        let route = route.clone();
        use_effect_with((), move |()| {
            let listener = Closure::<dyn Fn()>::new(move || {
                route.set(Route::from_hash(
                    &window().location().hash().unwrap_or_default(),
                ));
            });
            let _ = window()
                .add_event_listener_with_callback("hashchange", listener.as_ref().unchecked_ref());
            move || {
                let _ = window().remove_event_listener_with_callback(
                    "hashchange",
                    listener.as_ref().unchecked_ref(),
                );
            }
        });
    }
    let go = {
        let route = route.clone();
        Callback::from(move |next: Route| {
            let _ = window().location().set_hash(&next.hash());
            route.set(next);
        })
    };

    let Some(token) = (*token).clone() else {
        return html! {
            <main class="empty">
                <h1>{"vetinari"}</h1>
                <p>{"This page needs its token. Open the URL "}<code>{"vetinarid web"}</code>{" prints."}</p>
            </main>
        };
    };
    let token: Rc<str> = token.into();
    let body = match (*route).clone() {
        Route::List => html! { <RunListPage token={token.clone()} go={go.clone()} /> },
        Route::Run(run) => {
            html! { <RunPage key={run.clone()} token={token.clone()} run={run.clone()} go={go.clone()} /> }
        }
        Route::Preview => html! { <PreviewPage token={token.clone()} /> },
    };
    let nav = |label: &'static str, to: Route| {
        let go = go.clone();
        let current = *route == to;
        html! {
            <button class={classes!("tab", current.then_some("current"))}
                onclick={Callback::from(move |_| go.emit(to.clone()))}>{label}</button>
        }
    };
    html! {
        <>
            <header>
                <h1>{"vetinari"}</h1>
                <nav>{nav("Runs", Route::List)}{nav("Preview", Route::Preview)}</nav>
            </header>
            <main>{body}</main>
        </>
    }
}

/// Follow a server-sent event stream at `url`, handing each event to
/// `on_event`, until the URL changes or the component goes.
#[hook]
fn use_event_stream<T>(url: String, on_event: Callback<T>, on_lost: Callback<()>)
where
    T: DeserializeOwned + 'static,
{
    use_effect_with(url, move |url| {
        let mut source = EventSource::new(url).ok();
        let closed = Rc::new(std::cell::Cell::new(false));
        if let Some(Ok(mut events)) = source.as_mut().map(|source| source.subscribe("message")) {
            let closed = closed.clone();
            wasm_bindgen_futures::spawn_local(async move {
                while let Some(next) = events.next().await {
                    match next {
                        Ok((_, message)) => {
                            if let Some(event) = message
                                .data()
                                .as_string()
                                .and_then(|text| serde_json::from_str(&text).ok())
                            {
                                on_event.emit(event);
                            }
                        }
                        Err(_) => {
                            if !closed.get() {
                                on_lost.emit(());
                            }
                            break;
                        }
                    }
                }
            });
        }
        move || {
            closed.set(true);
            if let Some(source) = source {
                source.close();
            }
        }
    });
}

struct ListState(RunList);

impl Reducible for ListState {
    type Action = ListEvent;

    fn reduce(self: Rc<Self>, event: ListEvent) -> Rc<Self> {
        let mut list = self.0.clone();
        list.apply(event);
        Rc::new(Self(list))
    }
}

#[derive(Properties, PartialEq)]
struct ListProps {
    token: Rc<str>,
    go: Callback<Route>,
}

#[component]
fn RunListPage(props: &ListProps) -> Html {
    let list = use_reducer(|| ListState(RunList::default()));
    let lost = use_state(|| false);
    {
        let list = list.clone();
        let found = lost.clone();
        let lost = lost.clone();
        use_event_stream(
            logic::api_url("/api/runs", &props.token, &[]),
            Callback::from(move |event| {
                found.set(false);
                list.dispatch(event);
            }),
            Callback::from(move |()| lost.set(true)),
        );
    }
    let entries = list.0.entries();
    html! {
        <section class="runs">
            if *lost { <p class="problem">{"Lost the daemon; retrying."}</p> }
            if list.0.sign_in_needed() {
                <p class="problem">{"vetinarid needs signing in: run "}<code>{"vetinarid login"}</code></p>
            }
            if entries.is_empty() {
                <p class="empty">{"No runs. Start one from the TUI with "}<code>{"SPC w f"}</code>{"."}</p>
            } else {
                <ul class="run-list">
                    { for entries.into_iter().map(|entry| {
                        let go = props.go.clone();
                        let run = entry.run.clone();
                        html! {
                            <li class={classes!("run-row", status_class(entry.status))}
                                onclick={Callback::from(move |_| go.emit(Route::Run(run.clone())))}>
                                <span class="ticket">{entry.ticket.clone().unwrap_or_else(|| entry.run.clone())}</span>
                                <span class="title">{entry.title.clone().unwrap_or_default()}</span>
                                <Strip phases={entry.phases.clone()} />
                                { needs_badge(entry.needs_you.as_ref()) }
                            </li>
                        }
                    }) }
                </ul>
            }
        </section>
    }
}

const fn status_class(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Active => "active",
        RunStatus::Completed => "completed",
        RunStatus::Failed => "failed",
        RunStatus::Cancelled => "cancelled",
    }
}

fn needs_badge(needs: Option<&NeedsYou>) -> Html {
    match needs {
        Some(NeedsYou::Gate { gate, .. }) => {
            html! { <span class="badge">{format!("waiting: {gate}")}</span> }
        }
        Some(NeedsYou::Question { .. }) => {
            html! { <span class="badge">{"waiting: a question"}</span> }
        }
        None => html! {},
    }
}

#[derive(Properties, PartialEq)]
struct StripProps {
    phases: Vec<Phase>,
}

#[component]
fn Strip(props: &StripProps) -> Html {
    html! {
        <span class="strip">
            { for props.phases.iter().map(|phase| html! {
                <span class={classes!("phase", logic::mark_class(phase.mark))}>
                    {&phase.name}{&phase.glyph}
                    { phase.round.map(|round| format!("{}/{}", round.iteration, round.bound)).unwrap_or_default() }
                </span>
            }) }
        </span>
    }
}

#[derive(Properties, PartialEq)]
struct RunProps {
    token: Rc<str>,
    run: String,
    go: Callback<Route>,
}

#[component]
fn RunPage(props: &RunProps) -> Html {
    let view = use_state(|| None::<RunView>);
    let rounds = use_state(Rounds::default);
    let selected = use_state(|| None::<String>);
    let message = use_state(|| None::<String>);
    let note = use_state(|| None::<GateAction>);

    let mut query = Vec::new();
    if let Some(value) = rounds.query() {
        query.push(("rounds", value));
    }
    let url = logic::api_url(&format!("/api/runs/{}", props.run), &props.token, &query);
    {
        let view = view.clone();
        let message = message.clone();
        use_event_stream(
            url,
            Callback::from(move |next: RunView| view.set(Some(next))),
            Callback::from(move |()| {
                message.set(Some("Lost this run; is it dismissed?".to_owned()))
            }),
        );
    }

    let Some(current) = (*view).clone() else {
        return html! { <p class="empty">{"Loading the run…"}</p> };
    };

    let on_round = {
        let rounds = rounds.clone();
        Callback::from(move |(node, round, reached): (String, u32, u32)| {
            rounds.set((*rounds).clone().with(&node, round, reached));
        })
    };
    let on_select = {
        let selected = selected.clone();
        Callback::from(move |node: String| selected.set(Some(node)))
    };
    let post = {
        let token = props.token.clone();
        let message = message.clone();
        move |path: String, body: serde_json::Value, done: &'static str| {
            let url = logic::api_url(&path, &token, &[]);
            let message = message.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let sent = match Request::post(&url).json(&body) {
                    Ok(request) => request.send().await,
                    Err(error) => Err(error),
                };
                message.set(Some(match sent {
                    Ok(response) if response.ok() => done.to_owned(),
                    Ok(response) => {
                        format!("Refused: {}", response.text().await.unwrap_or_default())
                    }
                    Err(error) => format!("Could not reach vetinarid: {error}"),
                }));
            });
        }
    };

    let entry = &current.entry;
    let run = props.run.clone();
    let gate = match &entry.needs_you {
        Some(needs @ NeedsYou::Gate { instance, .. }) => {
            let instance = instance.clone();
            let actions = logic::gate_actions(needs);
            html! {
                <div class="gate">
                    <h3>{"Waiting on you"}</h3>
                    { for actions.into_iter().map(|action| {
                        let post = post.clone();
                        let note = note.clone();
                        let path = format!("/api/runs/{run}/gates/{instance}");
                        let label = action.label;
                        html! {
                            <button onclick={Callback::from(move |_| {
                                if action.takes_note {
                                    note.set(Some(action.clone()));
                                } else {
                                    post(path.clone(), serde_json::to_value(&action.answer).unwrap_or_default(), "Answered");
                                }
                            })}>{label}</button>
                        }
                    }) }
                    if (*note).is_some() {
                        <NoteForm on_send={{
                            let post = post.clone();
                            let note = note.clone();
                            let path = format!("/api/runs/{run}/gates/{instance}");
                            Callback::from(move |text: String| {
                                note.set(None);
                                let answer = GateAnswer::Recheck { note: Some(text) };
                                post(path.clone(), serde_json::to_value(&answer).unwrap_or_default(), "Answered");
                            })
                        }} />
                    }
                </div>
            }
        }
        Some(NeedsYou::Question { question, .. }) => html! {
            <div class="gate"><h3>{"The agent asks"}</h3><p>{question}</p>
            <p class="hint">{"Answer it from the TUI."}</p></div>
        },
        None => html! {},
    };

    let actions = if entry.status == RunStatus::Active {
        let post = post.clone();
        let path = format!("/api/runs/{}/cancel", props.run);
        html! {
            <button class="danger" onclick={Callback::from(move |_| {
                if window().confirm_with_message("Cancel this run?").unwrap_or(false) {
                    post(path.clone(), serde_json::json!({}), "Cancelling");
                }
            })}>{"Cancel run"}</button>
        }
    } else {
        let post = post.clone();
        let go = props.go.clone();
        let path = format!("/api/runs/{}/dismiss", props.run);
        let dismiss = move |remove_worktree: bool| {
            let post = post.clone();
            let go = go.clone();
            let path = path.clone();
            Callback::from(move |_| {
                let ask = if remove_worktree {
                    "Dismiss this run and remove its worktree?"
                } else {
                    "Dismiss this run?"
                };
                if window().confirm_with_message(ask).unwrap_or(false) {
                    post(
                        path.clone(),
                        serde_json::to_value(Dismiss { remove_worktree }).unwrap_or_default(),
                        "Dismissed",
                    );
                    go.emit(Route::List);
                }
            })
        };
        html! {
            <>
                <button onclick={dismiss(false)}>{"Dismiss"}</button>
                <button class="danger" onclick={dismiss(true)}>{"Dismiss and remove worktree"}</button>
            </>
        }
    };

    let selected_node = (*selected).as_ref().and_then(|node| {
        current
            .drawing
            .nodes
            .iter()
            .find(|drawn| &drawn.node == node)
    });

    html! {
        <section class="run">
            <div class="run-head">
                <button class="back" onclick={{ let go = props.go.clone(); Callback::from(move |_| go.emit(Route::List)) }}>{"← Runs"}</button>
                <h2>
                    <span class="ticket">{entry.ticket.clone().unwrap_or_else(|| entry.run.clone())}</span>
                    {" "}{entry.title.clone().unwrap_or_default()}
                </h2>
                <Strip phases={entry.phases.clone()} />
                <span class={classes!("status", status_class(entry.status))}>{status_class(entry.status)}</span>
            </div>
            if let Some(text) = (*message).clone() { <p class="message">{text}</p> }
            <div class="run-body">
                <Graph drawing={current.drawing.clone()} on_round={on_round}
                    selected={(*selected).clone()} on_select={on_select} />
                <aside>
                    {gate}
                    { node_detail(selected_node) }
                    if !current.questions.is_empty() {
                        <div class="notes"><h3>{"Preflight asks"}</h3>
                            <ul>{ for current.questions.iter().map(|question| html! { <li>{question}</li> }) }</ul></div>
                    }
                    if let Some(findings) = &current.findings {
                        <div class="notes"><h3>{format!("Review round {} found", findings.round)}</h3>
                            <ul>{ for findings.items.iter().map(|finding| html! {
                                <li>
                                    if let Some(file) = &finding.file {
                                        <code>{ match finding.line { Some(line) => format!("{file}:{line}"), None => file.clone() } }</code>{" "}
                                    }
                                    {&finding.text}
                                </li>
                            }) }</ul></div>
                    }
                    <div class="actions">{actions}</div>
                </aside>
            </div>
        </section>
    }
}

fn node_detail(node: Option<&ViewNode>) -> Html {
    let Some(node) = node else {
        return html! { <p class="hint">{"Select a node to see where it is."}</p> };
    };
    html! {
        <div class="node-detail">
            <h3>{&node.node}</h3>
            { match &node.state {
                Some(state) => html! {
                    <dl>
                        <dt>{"instance"}</dt><dd>{&state.instance}</dd>
                        <dt>{"status"}</dt><dd>{&state.status}</dd>
                        if let Some(detail) = &state.detail { <dt>{"detail"}</dt><dd>{detail}</dd> }
                        if let Some(attempt) = state.attempt { <dt>{"attempt"}</dt><dd>{attempt}</dd> }
                    </dl>
                },
                None => html! { <p>{"Not reached at this round."}</p> },
            } }
        </div>
    }
}

#[derive(Properties, PartialEq)]
struct NoteProps {
    on_send: Callback<String>,
}

#[component]
fn NoteForm(props: &NoteProps) -> Html {
    let text = use_node_ref();
    let on_send = props.on_send.clone();
    let send = {
        let text = text.clone();
        Callback::from(move |_| {
            if let Some(area) = text.cast::<HtmlTextAreaElement>() {
                let value = area.value();
                if !value.trim().is_empty() {
                    on_send.emit(value);
                }
            }
        })
    };
    html! {
        <div class="note">
            <textarea ref={text} placeholder="What changed, for preflight to read" />
            <button onclick={send}>{"Send"}</button>
        </div>
    }
}

#[derive(Properties, PartialEq)]
struct GraphProps {
    drawing: Drawing,
    #[prop_or_default]
    on_round: Option<Callback<(String, u32, u32)>>,
    #[prop_or_default]
    selected: Option<String>,
    #[prop_or_default]
    on_select: Option<Callback<String>>,
}

const MARGIN: u32 = 24;

#[component]
fn Graph(props: &GraphProps) -> Html {
    let drawing = &props.drawing;
    let view_box = format!(
        "-{MARGIN} -{MARGIN} {} {}",
        drawing.width + 2 * MARGIN,
        drawing.height + 2 * MARGIN
    );
    html! {
        <svg class="graph" viewBox={view_box} role="img" aria-label="the run's graph">
            <defs>
                <marker id="arrow" viewBox="0 0 10 10" refX="9" refY="5"
                    markerWidth="7" markerHeight="7" orient="auto-start-reverse">
                    <path d="M0 0 L10 5 L0 10 z" class="arrowhead" />
                </marker>
            </defs>
            { for drawing.nodes.iter().filter(|node| matches!(node.kind, NodeKind::Loop { .. })).map(|node| loop_box(node, props.on_round.as_ref())) }
            { for drawing.edges.iter().map(edge) }
            { for drawing.nodes.iter().filter(|node| matches!(node.kind, NodeKind::Step)).map(|node| step_box(node, props.selected.as_deref() == Some(node.node.as_str()), props.on_select.as_ref())) }
        </svg>
    }
}

fn loop_box(node: &ViewNode, on_round: Option<&Callback<(String, u32, u32)>>) -> Html {
    let NodeKind::Loop {
        reached,
        selected,
        bound,
    } = node.kind
    else {
        return html! {};
    };
    let rect = node.rect;
    let round = if reached == 0 {
        format!("up to {bound} rounds")
    } else {
        format!("round {selected}/{bound}")
    };
    let step = |to: u32, glyph: &'static str, x: u32| match on_round {
        Some(on_round) if reached > 1 && to >= 1 && to <= reached && to != selected => {
            let on_round = on_round.clone();
            let name = node.node.clone();
            html! {
                <text class="round-step" x={x.to_string()} y={(rect.y + 19).to_string()}
                    onclick={Callback::from(move |_| on_round.emit((name.clone(), to, reached)))}>{glyph}</text>
            }
        }
        _ => html! {},
    };
    html! {
        <g class={classes!("loop", logic::mark_class(node.mark))}>
            <rect x={rect.x.to_string()} y={rect.y.to_string()} width={rect.width.to_string()}
                height={rect.height.to_string()} rx="10" />
            <text class="loop-name" x={(rect.x + 12).to_string()} y={(rect.y + 19).to_string()}>
                {format!("{} · {round}", node.node)}
            </text>
            { step(selected.saturating_sub(1), "‹", rect.x + rect.width - 40) }
            { step(selected + 1, "›", rect.x + rect.width - 22) }
        </g>
    }
}

fn step_box(node: &ViewNode, selected: bool, on_select: Option<&Callback<String>>) -> Html {
    let rect = node.rect;
    let onclick = on_select.map(|on_select| {
        let on_select = on_select.clone();
        let name = node.node.clone();
        Callback::from(move |_: MouseEvent| on_select.emit(name.clone()))
    });
    let detail = node
        .state
        .as_ref()
        .map(|state| match &state.detail {
            Some(detail) => format!("{} · {detail}", state.status),
            None => state.status.clone(),
        })
        .unwrap_or_default();
    html! {
        <g class={classes!("step", logic::mark_class(node.mark), selected.then_some("selected"))} {onclick}>
            <title>{format!("{} {}", node.node, detail)}</title>
            <rect x={rect.x.to_string()} y={rect.y.to_string()} width={rect.width.to_string()}
                height={rect.height.to_string()} rx="6" />
            <text class="glyph" x={(rect.x + 12).to_string()} y={(rect.y + rect.height / 2 + 5).to_string()}>{&node.glyph}</text>
            <text class="name" x={(rect.x + 30).to_string()} y={(rect.y + rect.height / 2 + 5).to_string()}>{&node.node}</text>
        </g>
    }
}

fn edge(edge: &ViewEdge) -> Html {
    let label = edge.when.as_ref().zip(logic::label_at(&edge.points));
    html! {
        <g class="edge">
            <path d={logic::path(&edge.points)} marker-end="url(#arrow)" />
            if let Some((when, at)) = label {
                <text class="edge-label" x={at.x.to_string()} y={(at.y.saturating_sub(5)).to_string()}>{when}</text>
            }
        </g>
    }
}

#[derive(Properties, PartialEq)]
struct PreviewProps {
    token: Rc<str>,
}

#[component]
fn PreviewPage(props: &PreviewProps) -> Html {
    let intake = use_state(|| 3_u32);
    let review = use_state(|| 3_u32);
    let drawing = use_state(|| None::<Result<Drawing, String>>);
    {
        let drawing = drawing.clone();
        let url = logic::api_url(
            "/api/preview",
            &props.token,
            &[
                ("intake", intake.to_string()),
                ("review", review.to_string()),
            ],
        );
        use_effect_with(url, move |url| {
            let url = url.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let fetched = match Request::get(&url).send().await {
                    Ok(response) if response.ok() => response
                        .json::<Drawing>()
                        .await
                        .map_err(|error| error.to_string()),
                    Ok(response) => Err(response.text().await.unwrap_or_default()),
                    Err(error) => Err(error.to_string()),
                };
                drawing.set(Some(fetched));
            });
        });
    }
    let number = |label: &'static str, value: UseStateHandle<u32>| {
        let shown = *value;
        html! {
            <label>{label}
                <input type="number" min="1" max="20" value={shown.to_string()}
                    onchange={Callback::from(move |event: Event| {
                        if let Some(next) = event
                            .target_dyn_into::<HtmlInputElement>()
                            .and_then(|input| input.value().parse::<u32>().ok())
                        {
                            value.set(next);
                        }
                    })} />
            </label>
        }
    };
    html! {
        <section class="preview">
            <h2>{"feature-from-ticket"}</h2>
            <p class="hint">{"What every run of the template looks like before it starts. Nothing is registered."}</p>
            <div class="controls">{number("Intake rounds ", intake)}{number("Review rounds ", review)}</div>
            { match &*drawing {
                None => html! { <p class="empty">{"Loading…"}</p> },
                Some(Ok(drawing)) => html! { <Graph drawing={drawing.clone()} /> },
                Some(Err(problem)) => html! { <p class="problem">{problem}</p> },
            } }
        </section>
    }
}
