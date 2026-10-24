# Development Workflows on autognomus

Date: 2026-10-25
Status: Draft
Scope: The vetinari side of running development workflows on the autognomus workflow engine:
the templates, the step worker, the daemon it runs in, human gates, run visibility, and the
socket the TUI and a later web UI use to reach it.
Engine side: autognomus `docs/superpowers/specs/2026-10-25-external-tasks-loops-and-run-visibility-design.md`
(cited below as "engine spec"). Section 13 of that spec leaves the worker, its daemon and its
views to this document.

## 1. Purpose

The developer wants to hand a ticket to vetinari and get a pull request back, without
babysitting each step: a preflight that checks the ticket carries what an implementation
needs and asks a person when it does not, an implementation by a coding agent following TDD,
and a review that can send the work back. While that runs they want to see which phase it is
in and what is running in parallel, from the same queue pane they already use.

vetinari can already do each move on its own. `WorkflowEngine` cuts a worktree, opens a space
and starts an agent (`application/workflows.rs`), `AgentRegistry` tracks the agent, and the
queue folds agent work into the item it is on. What it cannot do is chain those moves, survive
its own restart part way through, or let a review send work back. The engine spec gives it a
durable graph to hang them on. This document is the part that runs on the developer's machine.

## 2. Decisions

The first six rows were made before this document and are not reopened here.

| Decision | Choice |
|---|---|
| Workflow engine | autognomus, through `External` and `Loop` nodes (engine spec 4.1) |
| How vetinari reaches it | Gateway HTTP only. NATS is the platform's internal bus |
| Where the worker runs | A new daemon, `vetinarid`, in a new crate `vetinari-daemon` |
| What moves into the daemon now | Dev workflows only. The attention queue stays in the TUI process |
| How runs reach the developer | As attention queue items, delivered over a local socket |
| Review rework | The engine's `Loop` node |
| Wire types | Mirrored in `vetinari-adapters`, not imported from autognomus (5.4) |
| Definitions | One definition compiled per run, deleted when the run is dismissed (4.4) |
| Template compilation | A pure function in the domain to a vetinari graph type; the adapter maps it to JSON |
| Step result | A JSON file the skill writes into the worktree, one per attempt (6) |
| Missing or invalid result | Permanent failure. A rerun of the same skill will not fix it |
| Rerun | Idempotent: an attempt adopts the run's existing worktree and branch |
| Task queues | Named per machine, so a step lands where its worktree is (4.5) |
| "Needs info, then recheck" | An intake `Loop` around preflight and its gate (4.3) |
| Abandoning a run | Cancel the run. Never a branch label |
| Local state in the daemon | A lease journal only. Runs are rediscovered from the gateway (7.7) |
| Daemon core | `vetinari-core` stays tokio-free; scheduling decisions are pure functions of the clock |
| Token | `vetinarid` owns the autognomus credential and is the only process that refreshes it |
| Socket | Unix domain socket, newline-delimited JSON, request/response plus streams (10) |
| Watching runs | One SSE stream for all runs, graph fetched on start, gap and `resync` |

### 2.1 Vocabulary

`application/workflows.rs` already calls a one-keypress job a workflow. To keep the two apart:

| Term | Means |
|---|---|
| workflow | The existing keypress job in `WorkflowEngine` (`start_pr_review`, `start_feedback`) |
| dev workflow | A multi-step template such as `FeatureFromTicket`, run on the engine |
| run | One execution of a dev workflow, identified by the engine's `RunId` |
| step | One `External` node a vetinari worker executes: preflight, implement, review, open-pr, or a gate |
| gate | A step whose worker is a person, surfaced through the queue |
| phase | What the strip shows: a group of steps the developer thinks of as one stage |

The new modules are named `devflow` so neither name has to change now.

## 3. Architecture

```
  vetinari (TUI)              vetinarid                                  autognomus gateway
  --------------              ---------                                  ------------------
  queue pane  <--runs.watch-- ClientApi --+-- StepWorker --claim/hb/--->  /v1/tasks/*
  RET popup   <--run.watch--- (socket)    |    |         complete/fail
  keys --------run.start----> ...         |    +-- WorkflowEngine (worktree, space, agent)
               gate.answer                |    +-- AgentSupervisor (owns AgentRegistry)
               run.cancel                 +-- RunMirror <----SSE--------  /v1/runs/events
                                          |             <----graph-----   /v1/runs/{id}/graph
                                          +-- Launcher -----register----> /v1/workflows
                                                          -----start----> /v1/workflows/{id}/runs
                                          +-- GatewayAuth (OIDC device code, keyring)
```

The TUI keeps its own `QueueService`, sources and keypress workflows. The daemon is one more
source to it: a `DaemonRuns` adapter that holds a `runs.watch` stream open and reports each run
as an `AttentionItem`. A daemon that is not running is a `SourceProblem`, exactly as a GitHub
outage is today.

## 4. Domain

All of this lives in `vetinari-core/src/domain/devflow/` and does no I/O.

### 4.1 Identifiers

Parsed once at the boundary, then trusted.

```rust
/// `[a-z0-9][a-z0-9-]{0,63}`, the engine's rule (engine spec 4.1).
pub struct TaskQueue(String);
/// The engine's `"<run>~<instance>~<attempt>"` (13), parsed into its parts.
pub struct TaskId { run: RunId, instance: NodeInstance, attempt: NonZeroU32 }
/// A ULID the engine minted at claim time. Opaque to vetinari.
pub struct LeaseToken(String);
pub struct RunId(String);
/// A node id: non-empty, no `@` (engine spec 4.2).
pub struct NodeId(String);
/// `review` or `review@2` or `check@2.1`.
pub struct NodeInstance { node: NodeId, iterations: Vec<NonZeroU32> }
/// A branch label: non-empty, `[a-z0-9-]+`.
pub struct BranchLabel(String);
/// What this daemon calls itself in a claim, and the middle of its queue names.
/// `[a-z0-9][a-z0-9-]{0,48}`, so `vetinari-<worker>-human` is always a valid queue.
pub struct WorkerName(String);
/// `vetinari-<template>-<ulid>`. Section 4.4.
pub struct DefinitionId(String);
```

`NodeInstance` round-trips through its string form exactly as the engine spec defines it,
including the rule that a suffix counts only if it is digits and dots. A legacy id such as `a@b`
therefore reads as a top-level node and is then refused, because a vetinari `NodeId` never holds
`@` and vetinari never registers one. `TaskId` is parsed from
the claim reply, never assembled by vetinari, so a change in the engine's format is one parser.

### 4.2 Templates

A template is a closed enum, because every template needs its own compile function, its own
phase mapping and its own tests. A free-form definition language would be a second engine.

```rust
pub enum DevWorkflow {
    FeatureFromTicket(FeatureFromTicket),
}

pub struct FeatureFromTicket {
    pub ticket: TicketRef,                // tracker key, title, url, source id
    pub repo: RepoTarget,                 // local path and remote slug
    pub profile: ProfileId,               // whose routing, worktree root and credentials
    pub base: Option<BranchName>,         // default: the repository's default branch
    pub max_review_rounds: Rounds,        // 1..=MAX_LOOP_ITERATIONS (20)
    pub max_intake_rounds: Rounds,        // default 3
    pub agent: Option<AgentId>,           // pins every agent step, else routing decides
}

/// `1..=20`, the engine's `MAX_LOOP_ITERATIONS`. Out of range does not construct.
pub struct Rounds(NonZeroU32);
```

`TicketRef` is the item's source, the tracker key, title and url, so a run started from a queue
item carries exactly what the item showed. It does not wrap `IssueRef`: that type's status and
transitions would be stale in a step's config within minutes, and no step reads them.

### 4.3 The feature graph

```
  intake: Loop (exit preflight = ready, max = max_intake_rounds)
  +------------------------------------------------+
  | preflight --needs-info--> ask-info (gate)      |
  |    |                                           |
  |    +--ready--> (path ends, iteration drains)   |
  +------------------------------------------------+
     |                    |
    done              exhausted --> (no edge: run completes, "stopped at intake")
     |
     v
  rework: Loop (exit review = approved, max = max_review_rounds)
  +------------------------------------------------+
  | implement --implemented--> review              |
  +------------------------------------------------+
     |                    |
    done              exhausted
     |                    |
     |                    v
     |              review-stuck (gate) --open-anyway--+
     |                                                 |
     v                                                 v
  open-pr <--------------------------------------------+
```

**Needs info, then recheck.** The graph is acyclic outside loops, so "ask the person, then run
preflight again" has to be a loop. `intake` is that loop. Iteration `n` runs `preflight`. If it
says `ready`, the edge to `ask-info` does not match, the iteration drains, and the exit is met.
If it says `needs-info`, `ask-info` runs, the person updates the ticket and answers, the
iteration drains with the exit unmet, and iteration `n + 1` runs `preflight` again.

The ticket is the source of truth for what the ticket says, so the gate does not carry the
missing information into the graph. It carries an optional note. The loop's `previous` is the
exit node's output (engine spec 5.4), which is preflight's, not the gate's. The worker
therefore reads `ask-info@<n>`'s output from `GET /v1/runs/{id}` when it starts
`preflight@<n+1>` and puts the note into the prompt. That is one extra fetch per recheck,
against teaching the engine a second `previous`.

The alternative considered was unrolling: `preflight`, `ask-info`, `preflight-2`. It was
rejected for the reason the engine spec rejects it for review: the strip and the history would
say `preflight-2` for what the developer thinks of as preflight again.

**Review rework.** `rework`'s body is `implement -> review`, exit `review = approved`.
`implement@<n+1>` gets `previous` = `review@<n>`'s output, which is the findings it must
address. That is exactly the shape the engine built `previous` for.

**Exhaustion.** An exhausted `rework` is a person's decision, so it goes to the
`review-stuck` gate. `open-anyway` goes to `open-pr`. Abandoning is a cancel (2). An
exhausted `intake` has no edge: the run completes, and the run summary says where it stopped.

**Failure inside the body.** An agent that cannot implement reports a permanent failure and the
run fails. An agent that has a question mid-step is not a graph gate: it is
`AgentStatus::WaitingForInput`, surfaced on the run item and answered through
`AgentRegistry::answer` (8.3). Graph gates are decisions the workflow author planned for. Agent
questions are not planned.

**Two inbound edges into `open-pr`.** `open-pr` has edges from `rework` (`done`) and from
`review-stuck` (`open-anyway`). They are mutually exclusive, and the engine queues a successor
per matched edge (parent engine spec 5.2), so it runs once. The engine accepts a non-join node
with several inbound edges and queues it once per matched edge (13).

### 4.4 Compiling

```rust
pub fn compile(
    workflow: &DevWorkflow,
    id: DefinitionId,
    queues: &WorkerQueues,
    policy: &StepPolicies,
) -> WorkflowGraph;

pub struct WorkflowGraph {
    pub id: DefinitionId,
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

pub enum GraphNode {
    External { id: NodeId, queue: TaskQueue, config: StepConfig, policy: StepPolicy },
    Loop { id: NodeId, body: NonEmpty<NodeId>, exit: LoopExit, max_iterations: Rounds },
}

pub struct StepPolicy {
    pub lease: LeaseSecs,              // >= 30, the engine's floor
    pub deadline: Option<DeadlineSecs>, // whole seconds, as the wire carries them
    pub attempts: NonZeroU32,
}

pub struct GraphEdge { pub from: NodeId, pub to: NodeId, pub when: Option<BranchLabel> }
```

`compile` is total: every input it accepts produces a graph the engine will register, because
`Rounds`, `LeaseSecs` and `TaskQueue` cannot hold a value the engine rejects. The graph type
names only the two node kinds vetinari uses. Component, delay and join can be added when a
template needs them.

**One definition per run.** The ticket, repository and profile are per run, and so is
`max_review_rounds`, which is the loop's `max_iterations` and therefore structural. A
template-level definition would have to thread the ticket through every step's output and
could not vary the round count. So the launcher compiles a definition per run, registers it
under `vetinari-feature-<ulid>`, and starts a run of it with input `{}`. Every step's `config`
carries what it needs.

The definition is deleted when the developer dismisses the finished run. Until then its
existence is how the daemon knows the run is one of its own (7.7). The run snapshots its
definition (engine spec 4.3), so deleting one never affects a run.

**Step policies** are configuration, with these defaults:

| Step | Queue | Lease | Deadline | Attempts | Why |
|---|---|---|---|---|---|
| preflight | agent | 300 s | 30 min | 3 | Cheap to rerun |
| implement | agent | 300 s | 4 h | 3 | Two spare attempts survive a lost lease or a crash beyond the lease |
| review | agent | 300 s | 1 h | 3 | As implement |
| open-pr | agent | 120 s | 10 min | 5 | Deterministic and idempotent |
| ask-info, review-stuck | human | 900 s | none | 10 | A person may take days; only daemon downtime burns attempts |

Lease expiry and deadlines both consume an attempt through the retry policy (engine spec 5.5),
which is why no agent step has `attempts: 1`. A restart inside the lease costs nothing (7.7).

### 4.5 Queues are per machine

A step's worktree is on the disk of the machine that cut it. If the laptop and the desktop both
claimed `vetinari-agent`, iteration 2 of `implement` could land on the machine without
iteration 1's commits. So the queues are named for the worker:

```rust
pub struct WorkerQueues { pub agent: TaskQueue, pub human: TaskQueue }
// vetinari-<worker>-agent, vetinari-<worker>-human, e.g. vetinari-hex-agent
```

The run is pinned to the machine that started it. Moving a run between machines is out of
scope (15).

### 4.6 Steps and their config

```rust
pub enum StepKind { Preflight, Implement, Review, OpenPr, Gate(GateKind) }
pub enum GateKind { InfoNeeded, ReviewStuck }

/// What every External node's `config` holds. Written by `compile`, parsed from the claim.
pub struct StepConfig {
    pub schema: StepSchema,             // 1
    pub step: StepKind,
    pub template: TemplateKind,         // feature-from-ticket
    pub ticket: TicketRef,
    pub repo: RepoTarget,
    pub profile: ProfileId,
    pub branch: BranchName,             // vetinari/<ticket-key>-<slug>, fixed at compile time
    pub base: Option<BranchName>,       // what the branch is cut from, else the default branch
    pub skill: Option<SkillId>,         // agent steps only
    pub agent: Option<AgentId>,
}
```

The branch is decided at compile time and written into every step, so no step has to agree with
another about where the work is. `base` is carried for the same reason: whichever step cuts the
worktree first needs it, and `FeatureFromTicket` is not otherwise visible to the worker.
A step's name in configs, results and outputs is `preflight`, `implement`, `review`, `open-pr`,
`info-needed` or `review-stuck`.

### 4.7 Outcomes

```rust
pub enum StepOutcome {
    Preflight(PreflightOutcome),
    Implement(ImplementOutcome),
    Review(ReviewOutcome),
    OpenPr(OpenPrOutcome),
    Gate(GateAnswer),
}

pub enum PreflightOutcome { Ready { summary: Summary },
                            NeedsInfo { questions: ShortList<Line> } }
pub enum ImplementOutcome { Implemented { summary: Summary } }
pub enum ReviewOutcome    { Approved { summary: Summary },
                            ChangesRequested { findings: ShortList<Finding> } }
pub enum OpenPrOutcome    { Opened { number: u64, url: Line } }
pub enum GateAnswer       { Recheck { note: Option<Line> },   // InfoNeeded
                            OpenAnyway }                       // ReviewStuck

impl StepOutcome {
    pub fn branch(&self) -> BranchLabel;   // ready, needs-info, implemented, approved, ...
}
```

`Summary`, `Line` and `Finding` are length-capped on construction (summary 2048 bytes, a line
500 bytes), and `ShortList` holds one to 20 entries, so both questions and findings are bounded
by type. A PR url is a `Line`: vetinari only shows it. Engine gap 4 says outputs are unbounded and the worker should keep them small.
This is where that happens.

The labels `compile` writes on edges and the labels `branch()` returns are the same constants.
A pure test walks every edge of every compiled template and asserts that some outcome of the
edge's source step produces its label, so a renamed label fails a test instead of silently
ending a path (engine spec: "an unmatched branch ends a path without failing the run").

### 4.8 Claimed tasks

```rust
pub struct ClaimedTask {
    pub id: TaskId,                     // names the run, instance and attempt
    pub lease: LeaseToken,
    pub expires_at: OffsetDateTime,
    pub queue: TaskQueue,
    pub config: StepConfig,
    pub input: StepInput,
}

pub enum StepInput {
    /// Outside a loop: the predecessor's output, unused by the current template.
    Plain,
    /// The first node of a loop body: the engine's envelope (engine spec 5.4).
    Iteration { iteration: NonZeroU32, previous: Option<StepOutcome> },
}
```

The claim reply's separate `run`, `instance` and `attempt` fields are not read: `TaskId` already
holds them and is the one the engine routes on, so keeping both would only make room for them to
disagree. An input is a loop envelope exactly when it is an object with an `iteration` key; no
step output or loop output carries one. `previous` is a `StepOutput` document (6), read back with
the result file's field rules, so the outcome a step completed with is the outcome the next
iteration sees.

A claim whose `config` does not parse as a `StepConfig` is failed permanently at once with the
parse error. It is a task some other tool put on a vetinari queue, or a schema vetinari no
longer reads, and neither improves on retry.

### 4.9 Run views

```rust
pub struct RunGraph {
    pub run: RunId,
    pub status: RunStatus,                  // Active | Completed | Failed | Cancelled
    pub seq: u64,
    pub nodes: Vec<RunNode>,                // id, kind, scope, step kind when External
    pub edges: Vec<GraphEdge>,
    pub instances: Vec<InstanceState>,
}

pub enum InstanceStatus {
    Ready, Running, Waiting { until: OffsetDateTime },
    Offered, Claimed { worker: String, expires_at: OffsetDateTime },
    Looping { iteration: NonZeroU32 }, Done { branch: Option<BranchLabel> },
    Failed { error: String }, Skipped, Cancelled,
}
```

Three pure functions sit on it:

| Function | Produces | Used by |
|---|---|---|
| `apply(&mut RunGraph, &RunEvent) -> Applied` | `Applied::Updated` or `Applied::Refetch` | `RunMirror` (9.1) |
| `summarise(&RunGraph, &LocalSteps) -> RunSummary` | the phase strip and what needs the developer | the queue item (9.2) |
| `lanes(&RunGraph, iteration selection) -> Vec<LaneRow>` | rows of the popup, parallel nodes side by side | the popup (9.3) |

`LocalSteps` is what only the daemon knows: the agent session on a claimed step, its question if
it is waiting, and its space. Phases are derived from step kinds, not node ids, so a second
template gets a strip without new code:

| Step kind | Phase |
|---|---|
| Preflight, Gate(InfoNeeded) | `pre` |
| Implement | `impl` |
| Review, Gate(ReviewStuck) | `rev` |
| OpenPr | `pr` |

## 5. Layout

### 5.1 Crates

```
crates/
  vetinari-core/src/
    domain/devflow/             ids, template, compile, step, outcome, result, task, run_view
    application/devflow/
      ports.rs                  TaskGateway, RunGateway, RunEvents, LeaseJournal, StepFiles
      launcher.rs               compile, register, start, refuse a second run per ticket
      worker.rs                 StepWorker: claim, run, heartbeat, complete, fail, adopt
      lease.rs                  pure heartbeat schedule and backoff
      gates.rs                  open gates and their answers
      mirror.rs                 RunMirror: graph plus events, seq rule
      api.rs                    ClientApi: what a socket client may ask
    application/workflows.rs    + start_step: worktree for the run's branch, space, agent
  vetinari-adapters/src/
    autognomus/
      wire.rs                   mirrored gateway DTOs (5.4)
      client.rs                 HttpGateway: TaskGateway + RunGateway over reqwest
      sse.rs                    RunEvents over GET /v1/runs/events
      oidc.rs                   discovery, device code, refresh for Provider::Autognomus
    devflow/
      journal.rs                FileLeaseJournal: atomic JSON file
      step_files.rs             FsStepFiles: read result files, exclude .vetinari/
    daemon/
      protocol.rs               socket frames, shared by server and client
      client.rs                 DaemonClient, and DaemonRuns: AttentionSource
  vetinari-daemon/              NEW
    src/bin/vetinarid.rs        argument parsing, then compose::run
    src/compose.rs              composition root
    src/socket.rs               inbound adapter: UnixListener, framing, ClientApi dispatch
    src/runtime.rs              tokio tasks: claim loops, heartbeats, agent polling, SSE
  vetinari-tui/
    src/ui.rs                   + phase strip, run popup
```

### 5.2 Hexagonal rules

- `domain/devflow` has no async, no I/O and no clock reads. It takes `OffsetDateTime`
  arguments. It does use `serde_json`, but only to read and write the JSON documents it
  defines itself (the result file, a step's config, a step's output): `StepResult::parse`
  takes bytes, so the parser has to live where the contract does.
- `application/devflow` defines every port it calls and depends on nothing outside
  `vetinari-core`. It is async through `async-trait`, as the existing services are.
- `vetinari-core` gains no tokio dependency. Waiting, timers and task spawning are the daemon
  runtime's. The application decides *what* is due from `Clock::now()` and the lease (7.3);
  `runtime.rs` decides *when* to ask. That is what lets every lease test run against
  `FixedClock` without sleeping.
- The socket protocol types live in `vetinari-adapters/src/daemon/protocol.rs` because both
  ends of the socket are adapters: the server is the daemon's inbound adapter and the client is
  the TUI's outbound one. `ClientApi` in the application speaks domain types and knows nothing
  about frames.
- `vetinari-daemon` is the only crate that depends on all the others. Its `compose.rs` is the
  daemon's composition root, as `vetinari-tui/src/startup.rs` is the TUI's.

### 5.3 What the TUI and daemon share

Both read the same `config.toml` and skill library. Each builds its own `AgentRegistry`,
`WorkflowEngine` and workspace targets. The TUI's registry keeps the keypress workflows; the
daemon's holds only step agents. A step's agent therefore never appears as a separate agent
item: it folds into its run item (8.1), which is the queue's existing rule for agents.

The daemon reloads the skill library at each claim rather than watching it. A claim is minutes
apart and the file is small.

### 5.4 Why vetinari mirrors the wire types

The repositories are separate and autognomus publishes no crates. Depending on
`autognomus-gateway` by git would pull axum, NATS clients and the engine's domain into
vetinari, and tie vetinari's build to an unreleased branch. More to the point, the gateway's
`PublicNode` exists *so that* callers do not depend on engine types: its doc comment says
publishing `WorkflowDraft` would make engine fields a public contract. The HTTP JSON is the
contract, so vetinari owns a copy of the part it uses.

`wire.rs` holds `PublicNode` (only `external` and `loop`), `PublicEdge`, `WorkflowBody`, the claim
reply, `RunGraphView`, the run event and the error body. Every wire enum that the gateway may
grow (`InstanceStatus`, event transition, `kind`) has a catch-all variant, so a new status
becomes a refetch and a log line rather than a decode error. The domain types have no
catch-all: the adapter decides what an unknown value means.

The copy is held honest by fixtures (11.3): JSON taken verbatim from the engine spec and from
the gateway's own tests, with the autognomus commit they came from.

The body vetinari registers always carries `"triggers": [{ "type": "manual" }]`: runs are
started by the launcher, never by a schedule or an event. A step's `config` is the
`StepConfig` document the domain writes (`StepConfig::to_json`), so the adapter never decides
what a step's config holds. The wire types vetinari only writes (`WorkflowBody`, the nodes and
edges) need no catch-all; that rule applies to the types it reads.

## 6. The step result contract

An agent step reports through a file, because a coding agent's process exit says only that it
stopped. The skill's prompt names the path, and the agent writes it last.

**Where.** `<worktree>/.vetinari/steps/<instance>-<attempt>.json`, for example
`.vetinari/steps/review@2-1.json`. In the worktree, because agent CLIs commonly refuse writes
outside their working directory. One file per attempt, so a stale result from iteration 1 or
from a lost attempt can never be read as the current one. `start_step` adds `.vetinari/` to the
worktree's `info/exclude`, so the file is never committed and the developer's `.gitignore` is
untouched. The path is also passed as `VETINARI_STEP_RESULT`.

**Shape.**

```json
{ "schema": 1, "outcome": "changes-requested",
  "summary": "Retry header is parsed but never honoured",
  "findings": [ { "file": "src/retry.rs", "line": 88, "text": "Retry-After ignored on 503" } ] }
```

`outcome` must be one of the labels the step kind can produce. Each label has its own required
fields: `ready`, `implemented` and `approved` need `summary`, `needs-info` non-empty `questions`,
`changes-requested` non-empty `findings` (a `summary` beside them is ignored), `opened` needs
`number` and `url`, `recheck` takes an optional `note`. The same field names are used for every
step kind, gates and open-pr included, so a step's output to the engine (below) is read back with
the same code when it comes round again as `previous`. Text over its cap is a parse error, not
truncated.

**Parsing** is `StepResult::parse(kind, &[u8]) -> Result<StepOutcome, ResultError>`, pure, in
the domain. The adapter only reads bytes.

| Agent ends | Result file | Worker does |
|---|---|---|
| `Succeeded` | parses | `complete` with the outcome |
| `Succeeded` | missing | `fail permanent`: "the skill wrote no result" |
| `Succeeded` | does not parse | `fail permanent` with the parse error |
| `Failed` | any | `fail transient` with the agent's message. The retry policy decides |
| `Cancelled` | any | `fail permanent`: someone stopped it on purpose |

**Output to the engine.**

```json
{ "branch": "changes-requested", "step": "review", "summary": "...",
  "findings": [ ... ], "git_branch": "vetinari/ENG-412-rate-limit-retries" }
```

`branch` is the only field the engine reads. The rest is for `previous`, for the popup, and for
`GET /v1/runs/{id}`.

**Idempotent rerun.** Every attempt of every step of a run uses the run's one branch and
worktree. `WorktreeManager::create` already adopts an existing worktree, and `start_step` reuses
the run's space when it is still open. A rerun is therefore "start the skill again in the same
place", and the skills say the worktree may hold work from an earlier attempt to continue from.

One shortcut makes a lost completion cheap. Before starting an agent for attempt `k`, the
worker looks for a valid result from an earlier attempt of the same instance whose recorded
`head` matches the worktree's current `HEAD`. If one exists, the work finished and only the
completion was lost, so it completes at once. This needs `WorktreeManager::head`, a one-line
port addition, and the worker records `head` beside the outcome when it reads a result.

**Skills.** Three built-ins join the library: `ticket-preflight`, `implement-tdd` and
`self-review`. Each template takes `{{ticket}}`, `{{result_path}}`, `{{iteration}}` and, where
it applies, `{{findings}}` or `{{note}}`. `implement-tdd` states the repository's TDD rule:
failing test first, then the code. Routing uses existing task classes: preflight is
`IssueSummary`, implement is `DeepCodeChange`, review is `PrReview`.

## 7. The step worker

`StepWorker` is an application service. The daemon runtime drives it.

### 7.1 Ports

```rust
#[async_trait]
pub trait TaskGateway: Send + Sync {
    async fn claim(&self, queue: &TaskQueue, worker: &WorkerName, wait: Duration)
        -> PortResult<Option<RawTask>>;
    async fn heartbeat(&self, task: &TaskId, lease: &LeaseToken) -> PortResult<LeaseReply>;
    async fn complete(&self, task: &TaskId, lease: &LeaseToken, output: &StepOutput)
        -> PortResult<TaskReply>;
    async fn fail(&self, task: &TaskId, lease: &LeaseToken, failure: &StepFailure)
        -> PortResult<TaskReply>;
}

pub enum LeaseReply { Extended { expires_at: OffsetDateTime }, Stale, Gone, NotFound }
pub enum TaskReply  { Accepted, Stale, Gone, NotFound }

#[async_trait]
pub trait LeaseJournal: Send + Sync {
    async fn load(&self) -> PortResult<Vec<HeldLease>>;
    async fn save(&self, held: &[HeldLease]) -> PortResult<()>;
}
```

`RawTask` (in `domain/devflow/task.rs`) holds the claim's parsed identifiers and keeps `config`
and `input` as `serde_json::Value` behind private fields, so the domain parser, not the adapter,
decides what a vetinari step is. The adapter's `ClaimReply::into_raw` parses only the
identifiers; one that does not parse is `PortError::Protocol`.

The HTTP adapter maps statuses onto these, and nothing else does:

| Gateway answer | Port result |
|---|---|
| `200` claim / `204` claim | `Some(task)` / `None` |
| `200 { expires_at }` | `Extended` |
| `409 { reason: stale }` / `409 { reason: gone }` | `Stale` / `Gone` |
| `404` | `NotFound` |
| `422` on complete | `PortError::Protocol`: a bug in vetinari's output, logged, then `fail permanent` |
| `401` | refresh the token once and retry; then `PortError::Unauthenticated` |
| `503`, connect error, timeout | `PortError::Unavailable`, which `is_transient` |

### 7.2 Claiming

One claim loop per queue. Each long-polls with `wait_seconds: 30`, the gateway's cap.

The agent queue's loop holds a permit from a semaphore of `[daemon] max_agent_steps` (default
2) before it claims. Pull is what makes that limit possible: the daemon never holds a task it
has no capacity to start, so a busy machine leaves work offered rather than claimed and idle.
The human queue's loop has no limit, because a gate costs nothing to hold.

On `Unavailable` a loop backs off 1, 2, 4 ... 60 seconds and resumes. On `Unauthenticated` it
stops and the daemon raises "autognomus needs sign-in" (12).

### 7.3 Heartbeats

Pure, in `lease.rs`:

```rust
/// When the next heartbeat is due: a third of the lease after the last extension.
pub fn next_heartbeat(lease: LeaseSecs, extended_at: OffsetDateTime) -> OffsetDateTime;
/// After a failed heartbeat: 2, 4, 8 s, capped at a sixth of the lease.
pub fn retry_after(lease: LeaseSecs, failures: u32) -> Duration;
```

With a 300 second lease that is a heartbeat every 100 seconds and two chances to fail before
the lease is in danger. A heartbeat that cannot reach the gateway is retried until it gets an
answer. It does not stop the agent on its own judgement: if the lease did expire, the engine's
answer will be `Stale`, and the work is in the worktree for the next attempt to adopt.

The run mirror's events (9.1) are used as a hint. A `node.cancelled` or `ag.run.cancelled` for a
run with a held lease triggers an immediate heartbeat, whose `Gone` is the authority. That
brings "the developer pressed cancel" down from a heartbeat interval to the SSE latency, without
trusting an event the engine says is best-effort.

### 7.4 Running a step

```
claimed
  parse config and input             -> unparseable: fail permanent, done
  journal: HeldLease { Starting }    -> durable before anything is created
  kind = agent step:
    earlier attempt's result valid and HEAD unchanged -> complete (6)
    WorkflowEngine::start_step       -> worktree on the run's branch, space, agent
    journal: HeldLease { Running { session, worktree, space } }
    AgentSupervisor watches session
  kind = OpenPr:
    push branch, find or open the PR -> complete with Opened
  kind = Gate:
    journal: HeldLease { AwaitingPerson }, publish to clients (8)
```

`start_step` is the new `WorkflowEngine` method. It is `run_isolated` with the run's
`WorktreeSpec` (a new `WorktreeSpec::for_run(repo, root, branch)`), the step's skill and
bindings, a space named for the ticket that is reused when it already exists, and two more
environment variables: `VETINARI_STEP_RESULT` and `VETINARI_RUN`.

`AgentSupervisor` is a single task in the runtime that owns the daemon's `AgentRegistry` and
calls `poll` every `[general] tick` (the TUI's cadence). The registry wants `&mut self` for
`spawn`, `cancel` and `answer`, and one owner is simpler than a lock held across awaits. It
reports each status change to the worker.

`OpenPr` needs no agent. It pushes the branch and opens the pull request through a new
`PullRequestPublisher` port, implemented with the existing GitHub adapter and credentials. It
first looks for an open PR from the branch, so a rerun finds the one it made.

### 7.5 Completing and failing

| Event | Call | On `Accepted` | On `Stale` | On `Gone` / `NotFound` | On `Unavailable` |
|---|---|---|---|---|---|
| result parsed | `complete` | drop the lease | drop | stop and clean up (7.6) | keep heartbeating, retry |
| no or bad result | `fail permanent` | drop | drop | clean up | retry |
| agent failed | `fail transient` | drop | drop | clean up | retry |

A completion waiting on an unreachable gateway stays in the journal as
`HeldLease { Completing { output } }` and is heartbeated as before, so the lease lives until the
completion lands. Dropping a lease means removing the journal entry. It never touches the
worktree, because the next step of the run uses it.

### 7.6 `Stale`, `Gone`, cancellation

| Reply | Agent | Space | Worktree | Journal |
|---|---|---|---|---|
| `Stale` | cancel | keep | keep: another attempt, almost always ours, will adopt it | drop |
| `Gone` | cancel | close | keep, and say where on the run item | drop |
| `NotFound` | cancel | close | keep | drop |

The worktree is never removed automatically. A cancelled run's branch may hold commits the
developer wants, and `WorktreeManager` cannot yet say whether a branch is pushed. The finished
run item offers "remove worktree" as an explicit action (8.2).

**Cancelling from vetinari** is `run.cancel` on the socket, which is `POST /v1/runs/{id}/cancel`.
The daemon does not stop its own agents first: the engine abandons the in-flight instances, the
SSE hint triggers a heartbeat, and `Gone` stops them through the same path as a cancel from
anywhere else. One path, whoever cancelled.

### 7.7 Restart and local state

The daemon persists one thing: the **lease journal**, `$XDG_STATE_HOME/vetinari/leases.json`,
written atomically through a temporary file and rename.

```rust
pub struct HeldLease {
    pub task: TaskId,
    pub lease: LeaseToken,
    pub expires_at: OffsetDateTime,
    pub queue: TaskQueue,
    pub step: StepKind,
    pub state: HeldState,   // Starting | Running { session, worktree, space }
                            // | AwaitingPerson | Completing { output }
}
```

On start, before any claim loop runs, the worker heartbeats every journal entry:

| Heartbeat | `HeldState` | Then |
|---|---|---|
| `Extended` | `Starting` or `Running` | Start the step again in the same worktree under the same lease and attempt. The agent was the old daemon's child and is gone; the shortcut in 6 catches a result it finished writing |
| `Extended` | `AwaitingPerson` | Republish the gate to clients |
| `Extended` | `Completing` | Send the completion again |
| `Stale`, `Gone`, `NotFound` | any | Drop the entry, as 7.6 |
| `Unavailable` | any | Keep the entry and retry, as a running heartbeat would |

A restart inside the lease therefore costs nothing but the agent's restart. A daemon down for
longer than the lease costs an attempt, and the claim reply lost in transit is covered by the
lease with no local state at all, as the engine spec intends.

**Runs need no local store.** The daemon's runs are the live definitions whose id starts with
`vetinari-` (`GET /v1/workflows`), and each one's runs (`GET /v1/workflows/{id}/runs`). Because a
definition lives until its run is dismissed (4.4), that list is exactly the runs the developer
still has in the queue. The engine is the source of truth for what a run is doing, and the daemon
has nothing to keep in step with it.

## 8. Human gates

### 8.1 A gate is a state of the run item, not a second item

The queue's rule is that agents fold into the work item they are on, and a gate is the run
asking the developer for something. One item per run keeps the gate next to its context and
means a run never appears twice. When a gate opens, the run item's state becomes
`ItemState::NeedsDecision`, a new state whose `needs_you()` is true. Its category is
`SourceCategory::AgentState`, so the queue's existing `AgentSignal::NeedsYou` rule brings it
back however it was parked. An agent's `WaitingForInput` on a step does the same.

### 8.2 Answering

The RET popup shows the gate's question and its choices, which are closed per `GateKind`:

| Gate | Shows | Choices | Becomes |
|---|---|---|---|
| `InfoNeeded` | preflight's `questions` | recheck, recheck with a note, abandon | `complete { branch: recheck, note }`, or `run.cancel` |
| `ReviewStuck` | the last review's findings, rounds used | open the PR anyway, abandon | `complete { branch: open-anyway }`, or `run.cancel` |

The TUI sends `gate.answer { run, instance, answer }`. The daemon checks the gate is still held,
completes it with the lease it holds, and replies once the gateway has accepted. An answer while
the gateway is unreachable is `Completing` in the journal and the reply says "sent when
autognomus is back". `Stale` or `Gone` replies "this gate is no longer open" and the popup
refreshes.

A gate is claimed as soon as it is offered and held by heartbeat until answered. Claiming at
answer time is not possible: `ClaimTask` takes the oldest task on a queue, not a named one.

## 9. Run visibility

### 9.1 The mirror

`RunMirror` holds a `RunGraph` per run the daemon owns, and applies engine spec 7.3:

1. Open `GET /v1/runs/events` (every run; the daemon filters to its own, since its workflow ids
   are per run and there is no prefix filter).
2. Then fetch `GET /v1/runs/{id}/graph` for each owned run.
3. Buffer events that arrive before a run's graph, then apply only those with `seq` greater than
   the graph's.
4. A `seq` gap for a run, an `event: resync`, a reconnect, or an `Applied::Refetch` from `apply`
   refetches that run's graph (all runs, for `resync` and reconnect).
5. A fetched graph older than the one held (lower `seq`) is discarded, so two refetches racing
   cannot move the view backwards.

`apply` updates an instance's status from a node event. It returns `Refetch` for
`loop.iteration` and for terminal run events, because `Skipped` is derived by the engine
(engine spec 6) and the mirror must not try to derive it a second way.

Refetches are coalesced to at most one in flight per run, with a 250 ms debounce. `seq` counts
only observable transitions (13), so a gap genuinely means a missed event; the debounce keeps a
burst of them cheap.

A run started by the launcher is added to the mirror from the `201` and its graph fetched at
once. Runs of other workflows on the same gateway are ignored.

### 9.2 The phase strip

The queue pane is about 60 by 12. A run item is one row, like every other item, with the strip
right-aligned where other items show their state tag:

```
 ▸ [run] ENG-412 Rate-limit retries   pre✓ impl●2/3 rev○ pr·
   [run] ENG-398 Paginate audit export   pre? impl· rev· pr·
   [pr]  #1204 Bump tokio to 1.54                     review
   [msg] Ana: can you look at the deploy?                 2m
```

| Glyph | Means |
|---|---|
| `✓` | done |
| `●` | an agent is running it on this machine |
| `◐` | offered or claimed elsewhere, not yet running here |
| `?` | needs you: an open gate or an agent question |
| `✗` | failed |
| `○` | next, not started |
| `·` | not reached |

A loop phase shows its iteration and bound while it runs (`impl●2/3`), and drops them once it
finishes on its first round (`pre✓`). The strip drops
from the right when the title needs the room, and the phase that needs the developer is never
the one dropped.

### 9.3 The popup

RET on a run item opens the run. It is a list of top-level phases. A loop is a group with one
tab per iteration, the latest selected, and its body's nodes beneath. Nodes that are in flight at
the same time in one scope are drawn as lanes:

```
┌ ENG-412 Rate-limit webhook retries ──────────────────────┐
│ feature-from-ticket · active · 1h12m                     │
│                                                          │
│ intake   ✓ ready on round 1                              │
│ rework   [1] [2]  · ·                       round 2 of 3 │
│   implement  ✓ 38m   claude-code                         │
│   review     ● 6m    claude-code     lease ok            │
│ open-pr  · not reached                                   │
│                                                          │
│ round 1 review: changes-requested                        │
│   src/retry.rs:88  Retry-After ignored on 503            │
│   src/retry.rs:112 no test for the jitter cap            │
│ ←/→ round  o space  l log  c cancel  esc close           │
└──────────────────────────────────────────────────────────┘
```

A gate is the same layout with the question in the lower half and the choices as keys:

```
┌ ENG-398 Paginate audit export ───────────────────────────┐
│ feature-from-ticket · needs you · 4m                     │
│                                                          │
│ intake   [1]                                round 1 of 3 │
│   preflight  ✓ needs-info                                │
│   ask-info   ? waiting for you                           │
│ rework   ·                                               │
│                                                          │
│ The ticket does not say:                                 │
│  1. Page size, or whether the client chooses it          │
│  2. Whether a cursor must survive a re-sort              │
│ Update the ticket, then:                                 │
│ r recheck  n recheck with note  x abandon  esc close     │
└──────────────────────────────────────────────────────────┘
```

Lanes, for a template that reviews in parallel (none does yet, but `lanes` handles it so the
first one that does needs no view work):

```
│ rework   [1]                                             │
│   implement      ✓ 38m                                   │
│   ├ review-tests   ● 4m   claude-code                    │
│   └ review-design  ◐ offered                             │
│   join           ·                                       │
```

`lanes` orders a scope's nodes by depth from the scope's entries. Nodes at the same depth are one
lane group. That is a property of the drawn graph, not of timing, so the layout does not jump as
instances start and finish.

The popup subscribes with `run.watch` while open and closes the stream on `esc`. `o` focuses the
step's space through the TUI's own workspace target, and `l` shows the agent's output tail, both
of which go through the daemon because it owns the session.

## 10. The socket

### 10.1 Transport

A Unix domain socket at `$XDG_RUNTIME_DIR/vetinari/vetinarid.sock`, in a directory created
`0700`. The filesystem is the authentication: only the developer's user can connect, which is
also who owns the autognomus token behind it. TCP on localhost would need a token of its own and
would be reachable by every local user.

Framing is newline-delimited JSON, one frame per line, at most 1 MiB. It needs only tokio and
serde_json, both already in the workspace, and it can be driven by hand with `socat` when
something looks wrong.

### 10.2 Frames

```json
{ "id": 7, "method": "run.start", "params": { ... } }          client -> daemon
{ "id": 7, "result": { ... } }                                  daemon -> client
{ "id": 7, "error": { "code": "conflict", "message": "..." } }  daemon -> client
{ "stream": 9, "event": { "type": "run", ... } }                daemon -> client, on a stream
{ "id": 10, "method": "stream.close", "params": { "stream": 9 } }
```

A method that opens a stream replies with `{ "stream": n }` and then pushes events tagged `n`.
Error `code` is a closed set: `invalid`, `not-found`, `conflict`, `unavailable`,
`unauthenticated`, `protocol`. Unknown fields are ignored in both directions, which is the whole
versioning policy until it is not enough.

### 10.3 Methods

| Method | Params | Result or stream |
|---|---|---|
| `hello` | `{ client, protocol: 1 }` | `{ daemon, protocol, worker }`; a protocol mismatch is `invalid` and the client says to restart the daemon |
| `runs.watch` | `{}` | stream: `snapshot { runs: [RunSummary] }`, then `run { RunSummary }` and `removed { run }` |
| `run.watch` | `{ run }` | stream: `graph { RunDetail }` on every change |
| `run.start` | `{ template: "feature-from-ticket", ticket, repo, profile, max_review_rounds? }` | `{ run }`; `conflict` if the ticket already has a live run |
| `run.cancel` | `{ run }` | `{}` once the gateway accepted |
| `run.dismiss` | `{ run, remove_worktree: bool }` | `{}`; deletes the definition, so the run leaves the queue |
| `gate.answer` | `{ run, instance, answer }` | `{ sent: bool }` |
| `agent.answer` | `{ run, instance, text }` | `{}` |
| `agent.log` | `{ run, instance, lines }` | `{ lines: [..] }` |
| `auth.status` | `{}` | `{ state, expires_at? }` |
| `auth.begin` | `{}` | stream: `challenge { user_code, verification_uri }`, then `granted` or `rejected` |

`RunSummary` is `{ run, ticket, title, status, phases: [PhaseCell], needs_you: NeedsYou?, started_at,
worktree? }`. `RunDetail` is the `lanes` rows plus findings and the open gate. Both are the
domain types of 4.9 serialised by `protocol.rs`, so the TUI renders what the daemon derived and
holds no workflow logic.

A future web UI needs no new methods, only a transport: an HTTP and SSE inbound adapter in
`vetinari-daemon` over the same `ClientApi`. That is why `ClientApi` is an application service
and the socket is only its first adapter.

### 10.4 The TUI side

`DaemonRuns` implements `AttentionSource`. It keeps a `runs.watch` stream open, caches the
latest summaries, and `fetch` returns the cache as items with
`ItemContext::Run(RunSummary)`. A broken stream reconnects with backoff, and while it is down
`fetch` returns `PortError::Unavailable`, which the queue already shows as a source problem and
already treats as "do not resolve what it reported" (`QueueService::absorb`).

Starting a run is a key on an issue item (`SPC w f`, feature from ticket). It sends `run.start`,
then `Decision::Delegate { item: <ticket item>, agent: <run item> }`, which is the queue's
existing way of saying "this item is being worked on elsewhere, wait on it". The ticket item
waits on the run item, and the run item resurfaces when it needs the developer.

The TUI never starts the daemon. `vetinarid` runs as a systemd user service (a unit file ships in
`docs/`), and `vetinarid --foreground` for development. A TUI that starts a daemon would have to
decide whether it owns it, and the answer would be wrong for whichever of the two clients started
second.

## 11. Testing

Every test is written red first. No test sleeps: leases and backoffs are pure functions of
`FixedClock`, and the runtime's timers are exercised only in the daemon crate, with tokio's
paused time.

### 11.1 Layers

| Layer | What is tested | Against |
|---|---|---|
| Domain | `TaskQueue`, `NodeInstance` (including `a@b` legacy ids), `TaskId` parsing; `Rounds` bounds; `compile` for every template; every compiled edge label is producible by its source step; `StepResult::parse` for every outcome, every missing field, oversize text, wrong schema; `StepInput` loop envelopes with and without `previous`; `apply` for every transition, gap, older graph; `summarise` and `lanes` for every status | Pure |
| Compile invariants | A test-only checker mirroring engine spec 4.4: no `@`, one body per node, exit in body, edges within scope, acyclic outside loops, `max_iterations <= 20`, `lease >= 30` | Pure, every template |
| Worker | claim then start then complete; capacity limits claiming; heartbeat cadence; heartbeat `Unavailable` retried and not fatal; `Stale` and `Gone` per 7.6; result missing, invalid, agent failed, agent cancelled; completion buffered while unavailable; restart re-adoption for every `HeldState`; earlier attempt's result adopted only when `HEAD` matches; unparseable config failed permanently; gate answered, gate answered after `Gone` | `FakeTaskGateway` (scripted replies, records calls), `MemoryLeaseJournal`, existing `FakeAgentEngine`, `FakeWorktreeManager`, `FakeWorkspaceTarget`, `FixedClock` |
| Launcher | compiles, registers, starts; second live run for a ticket is `conflict`; register succeeded but start failed deletes the definition | `FakeRunGateway` |
| Mirror | subscribe before fetch; buffered events applied above the graph's `seq`; gap and `resync` refetch; coalescing | `FakeRunEvents`, `FakeRunGateway` |
| Gateway adapter | every row of 7.1's status table; bearer header; task ids in paths unencoded; 401 then refresh then retry once; wire JSON of every compiled template equals its golden file; SSE parsing including `resync` and a dropped connection | existing `vetinari-adapters::testing::MockServer`, extended to stream a chunked body for SSE |
| OIDC | discovery, device code start and poll, refresh writing back to the store | `MockServer`, `MemoryCredentialStore` |
| Socket | frame round trip for every method; oversized frame rejected; stream close; two clients watching at once | an in-process `UnixListener` in a temp dir |
| TUI | strip glyphs and truncation at 60 columns; both popups; gate keys send the right answer | ratatui `TestBackend`, as `ui.rs` already uses |

### 11.2 Why no wiremock

The workspace has no HTTP mocking crate, and `vetinari-adapters/src/testing.rs` already has a
`MockServer` that every API adapter is tested with. Adding wiremock for one adapter would be two
ways of doing one thing. The extension it needs is a reply that streams lines without a
`content-length`, for SSE.

### 11.3 Contract fixtures

`crates/vetinari-adapters/tests/fixtures/autognomus/` holds the JSON the adapter must read and
write: the two node variants from engine spec 8.4, a claim reply, a `RunGraphView` for each
`InstanceStatus`, a node event for each transition, and each `409` body. Each file names the
autognomus commit it was taken from. When the gateway lands, the fixtures are regenerated from
its own tests rather than the spec, and any difference is a bug on one side to be settled before
merging.

A live test against a real gateway, run with podman compose under `$AGENTPOD_WORK_HOST`, is
`#[ignore]` and opt-in by environment variable. It drives one run of a two-step template through
real claims. It is the only test that can catch the engine and vetinari disagreeing about
something neither wrote down.

## 12. Authentication

The gateway takes an OIDC bearer token, validated against the issuer's JWKS for `iss`, `aud`
and `exp` (`autognomus-gateway/src/http/auth.rs`), single owner, no scopes.

**A new provider.** `Provider::Autognomus` joins the closed enum. Its `AuthSpec` cannot be the
existing `AuthMethod::OAuthDeviceCode`, whose URLs are `&'static str` because every current
provider has fixed endpoints. The autognomus issuer is whatever the developer deployed, so the
new method is `AuthMethod::OidcDeviceCode`, whose endpoints come from configuration through
discovery:

```toml
[autognomus]
gateway_url = "https://autognomus.example.net"
issuer      = "https://id.example.net/realms/home"
client_id   = "vetinari"
audience    = "autognomus"           # sent as the issuer requires, see below
scopes      = ["openid", "offline_access"]
worker      = "hex"                  # default: the hostname, as a slug
```

`oidc.rs` reads `<issuer>/.well-known/openid-configuration` for `device_authorization_endpoint`
and `token_endpoint`, then runs the existing RFC 8628 flow in `HttpDeviceCodeFlow` with those
endpoints. The credential is stored through the existing `CredentialStore` under
`CredentialKey::new(Provider::Autognomus, None)`.

**Who holds it.** `vetinarid` is the only process that reads, refreshes or writes this
credential. The TUI never asks for it. Refresh tokens are commonly rotated on use, and two
processes refreshing one credential would each invalidate the other's. `AuthService::token`
already refreshes on the way out and writes back; the daemon also refreshes at `NearingExpiry`
so a long-poll does not start with a token about to lapse, and forces one refresh on a `401`.

**Signing in.** `auth.begin` on the socket runs the device flow in the daemon and streams the
challenge to the TUI, which shows it in the existing auth wizard. `vetinarid login` does the same
on stdout for a first run before any client exists. A daemon with no usable token keeps its
socket up, reports `unauthenticated`, and surfaces an "autognomus needs sign-in" item through
`runs.watch`.

**Assumptions, flagged.** Each of these is a property of the deployed issuer, not of vetinari or
the gateway, and none has been verified:

1. The issuer supports the device authorization grant and advertises it in discovery.
2. The access token it issues is a JWT carrying a `kid`, `iss` and an `aud` equal to the
   gateway's `AUTOGNOMUS_OIDC_AUDIENCE`. How the audience is requested differs by issuer: an
   `audience` parameter on some, a client mapper on others, a scope on others. `audience` in the
   config is passed on the device request when set, and the setup notes per issuer go in the
   README.
3. It issues a refresh token for `offline_access`. Without one the developer signs in again
   whenever the access token expires, and a step that outlives it fails at its next heartbeat.

If the issuer cannot do 1, the fallback is a loopback redirect with PKCE served by `vetinarid`
on `127.0.0.1`, which this document does not design.

Keyring access from a daemon needs the user's Secret Service session. Under a systemd user unit
in a desktop session it has one. A headless machine has none, and the credential adapter has
no file fallback today; that is gap 7.

## 13. Engine spec points, resolved

Found while designing against the engine spec, and settled with the engine side before the
gateway endpoints are built.

| Point | Resolution | Effect on vetinari |
|---|---|---|
| `seq` gaps | `seq` increments exactly when the engine emits a run or node event. Heartbeats and other saves without an event do not bump it | A gap genuinely means a missed event (9.1) |
| Task id in a path | `TaskId` is `<run>~<instance>~<attempt>`. `~` is URL-unreserved, so `/v1/tasks/{task}/...` needs no encoding | `TaskId::parse` splits on the first and last `~`; the adapter puts the id in the path as is |
| Public node tagging | Internally tagged by `kind`, matching `PublicNode`: `{ "kind": "external", "id", "queue", "config", "lease_seconds", "deadline_seconds", "retry": { "attempts": N } }` and `{ "kind": "loop", "id", "body", "exit": { "node", "when" }, "max_iterations" }` | The wire shape vetinari writes, and its fixtures |
| Retry field | `attempts`, matching `RetrySpec` | Same |
| Inbound edges into a non-join | A non-join node may have several inbound edges; each matched edge queues it, so exclusive edges run it once | `open-pr` (4.3) |
| Outputs on `GET /v1/runs/{id}` | Keyed by instance string form, e.g. `ask-info@2` | The recheck note (4.3) is read from `ask-info@n`'s key |

## 14. Build order

Each slice is red, green, refactor, and leaves `cargo test`, `cargo clippy` and `cargo fmt`
clean.

**Buildable now**, against the engine spec and fakes, before any autognomus endpoint exists:

1. **Identifiers.** `TaskQueue`, `NodeId`, `NodeInstance`, `TaskId`, `LeaseToken`,
   `BranchLabel`, `Rounds`, `DefinitionId`.
2. **Step outcomes and the result contract.** `StepKind`, `StepOutcome`, `branch()`,
   `StepResult::parse`, length caps.
3. **The feature template.** `FeatureFromTicket`, `compile`, the label-producibility test and
   the engine-rules checker.
4. **Wire mapping.** `wire.rs` and `WorkflowGraph` to public JSON, against golden fixtures.
5. **Claimed tasks.** `StepConfig` and `StepInput` parsing from fixture claim replies.
6. **Leases.** `next_heartbeat`, `retry_after`, `LeaseJournal` and `FileLeaseJournal`.
7. **`start_step`.** `WorktreeSpec::for_run`, `.vetinari/` excluded, space reuse,
   `WorktreeManager::head`, in the existing `WorkflowEngine` tests.
8. **The worker** against `FakeTaskGateway`: 7.2 to 7.7 in that order.
9. **The HTTP gateway client** against `MockServer` and the status table of 7.1.
10. **Run views.** `RunGraph`, `apply`, `summarise`, `lanes`, and `RunMirror` against fakes.
11. **The daemon skeleton.** `vetinari-daemon`, `compose.rs`, the socket with `hello` and
    `runs.watch`, and a `--scripted <file>` mode that replays a recorded run so the TUI can be
    developed without a gateway.
12. **The TUI.** `DaemonRuns`, the strip, the popup and gate keys, driven by the scripted daemon.
    The `run-vetinari` skill gains a step to start it.
13. **Authentication.** `Provider::Autognomus`, `OidcDeviceCode`, discovery, `auth.begin`,
    `vetinarid login`, against `MockServer`.
14. **The launcher and `run.start`** against `FakeRunGateway`, and `SPC w f` with `Delegate`.
15. **`OpenPr`.** `PullRequestPublisher` and its GitHub adapter. Independent of the engine.

**Waits on autognomus:**

16. **SSE adapter, verified.** It can be written against `MockServer` in slice 10's spirit, but
    its framing is unverified until gateway task 9 serves `GET /v1/runs/events`.
17. **First live run**, preflight and implement only, no loops: needs engine tasks 2 to 4 and the
    task endpoints of task 9 (the engine spec's own "worker path").
18. **Loops live.** `intake` and `rework` end to end: needs engine task 7.
19. **Graph view live.** The popup on real data: needs engine task 8 and the graph endpoint.
20. **Cancel live**: needs engine task 5 and the cancel endpoint.
21. **Reminders as queue items.** `/v1/scheduled-events` delivers to a webhook today, which a
    laptop cannot receive. The gateway spec plans an `SseDeliverer` beside the webhook; when it
    exists, vetinarid subscribes and each fired event becomes a queue item through `runs.watch`
    (renamed `items.watch` then). Until it does, the same effect is available from what this
    document builds: a workflow with a schedule trigger and one gate on the human queue. That route
    is the one taken (16).

## 15. Known gaps and out of scope

**Gaps carried forward:**

1. **A run cannot move machines.** Queues are per worker (4.5). A laptop that is off holds its
   runs until it is back, and its leases expire and burn attempts meanwhile.
2. **One branch per run, one live run per ticket.** A second idea for the same ticket waits for
   the first run to be dismissed.
3. **Worktrees are never removed automatically** (7.6).
4. **Agent questions are not durable.** A `WaitingForInput` question lives in the agent process. A
   daemon restart restarts the step, and the agent asks again or does not.
5. **Gate answers are not attributed.** Single owner, so every answer is the owner's. A shared
   gateway would need the queue to be a scope (engine gap 2).
6. **Restarted steps lose agent context.** A restart within the lease restarts the agent in the
   same worktree with a fresh session; it continues from the files, not from its conversation.
7. **No headless keyring.** `vetinarid` on a machine without a Secret Service session cannot
   store its token (12).

**Out of scope:**

- The web UI itself, beyond the socket API it will use (10.3).
- Moving the attention queue into `vetinarid`.
- Templates other than `FeatureFromTicket`, and parallel reviewers.
- Moving tickets through their tracker's statuses as a run progresses.
- Dynamic fan-out (engine gap 6).
- A loopback-redirect sign-in for issuers without device code (12).

## 16. Open questions

1. **The issuer.** Which OIDC provider fronts the gateway, and does it meet the three assumptions
   of section 12? Slice 13 can be built against `MockServer` either way, but the first live run
   cannot happen until this is known.

Decided, with the defaults recorded here:

| Question | Decision |
|---|---|
| Worktree removal | Stays an explicit action on the finished run item (7.6). No "pushed?" query is needed |
| Reminders | A schedule-triggered workflow with one gate on the human queue. Nothing new is needed from autognomus |
