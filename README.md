# Vetinari

An orchestrator for coding agents, attention, and technical leadership.

Vetinari is a Rust TUI that puts one prioritised inbox in front of you -
pull requests, issues, messages, meetings, and the agents it has started on
your behalf - and turns a keypress into the whole workflow behind it: cut a
worktree, open a space, brief an agent, and tell you where it went.

It is named after the Patrician of Ankh-Morpork, who kept the city running by
knowing what deserved his attention and what did not.

## What it does

- **One prioritised feed.** GitHub, Jira, Slack, Google Calendar, and running
  agents in a single list, ranked by interruption cost: an agent waiting on an
  answer is wasting time right now; a meeting in fifteen minutes cannot be
  deferred; finished work sinks.
- **Doom Emacs keybindings.** Modal navigation (`hjkl`, `gg`, `G`, `/`) with a
  `<Space>` leader and a which-key popup, all rebindable in TOML or YAML.
- **Workflows, not links.** `<Space> p r` on a pull request fetches its branch,
  cuts a worktree, opens a space, and starts a review agent in it.
- **A feedback loop on itself.** `<Space> f f` captures what you were looking
  at - the selected row, the open spaces, the running agents, the tail of the
  log - and hands it to an agent in a fresh worktree of this repository.
- **Agent-agnostic.** Claude Code for codebase work, Gemini through `agy` for
  long-context reading, scripts for cheap summaries, chosen per class of work
  by a routing table you own.
- **Credentials in the OS keychain**, with token and OAuth device-code flows.

## Installing

```sh
cargo install --path crates/vetinari-tui     # or: cargo build --release
```

Needs Rust 1.88 or newer, `git`, and whichever agents and workspace backends
you point it at (`claude`, `agy`, `tmux`, Herdr, an editor).

## Using it

```sh
vetinari                      # open the interface
vetinari doctor               # check config, engines, and credentials
vetinari auth                 # list every provider's credential state
vetinari auth github          # paste a token (read from stdin)
vetinari auth google-calendar --device
vetinari config               # print where configuration is looked for
```

`vetinari --project ~/src/thing` points the feedback workflow at another
project; it defaults to the working directory.

### Keybindings

| Keys | Does |
| :--- | :--- |
| `hjkl`, `TAB` | move within a pane, and between panes |
| `gg`, `G`, `C-d`, `C-u` | top, bottom, page down, page up |
| `/`, `<Esc>` | filter the feed, clear the filter |
| `?` | every binding in force |
| `<Space> f f` | feedback / enhancement on this project |
| `<Space> p r` | review the selected pull request in a worktree |
| `<Space> p a`, `<Space> p c` | approve, comment |
| `<Space> p p`, `<Space> p l` | cycle profile, choose profile |
| `<Space> i u`, `<Space> i a`, `<Space> i s` | issue status, assign, standup summary |
| `<Space> c r` | reply to a message |
| `<Space> m m`, `<Space> m p`, `<Space> m s` | meetings, prepare, schedule |
| `<Space> a a`, `<Space> a u`, `<Space> a c`, `<Space> a r`, `<Space> a l` | agents: menu, unblock, cancel, re-run, output |
| `<Space> w w`, `<Space> w s`, `<Space> w d`, `<Space> w r` | spaces: menu, focus, close, run the agent in its space |
| `<Space> s e`, `<Space> s r` | edit a skill, run a skill |
| `<Space> h a`, `<Space> h k` | credential wizard, help |
| `<Space> q` | quit |

Inside an overlay: `<Esc>` cancels, `RET` submits a line, `C-s` submits a
paragraph, `TAB` moves between fields.

### Configuring

Copy [`docs/config.example.toml`](docs/config.example.toml) to
`~/.config/vetinari/config.toml`. Every section is optional - with no
configuration Vetinari starts with the Doom keymap, work and home profiles, a
tmux target, and no sources. YAML is accepted too.

Problems are reported all at once, with the path of each: a typo in a key
name is an error rather than a setting that silently does nothing.

## Architecture

Three crates, with dependencies pointing inward only:

| Crate | Holds |
| :--- | :--- |
| `vetinari-core` | the domain, the ports, and the application services. No I/O. |
| `vetinari-adapters` | git, processes, tmux, Herdr, editors, HTTP APIs, the keychain. |
| `vetinari-tui` | the state and decisions, the rendering, the event loop. |

Everything the outside world provides is a trait in
`vetinari_core::application::ports`, implemented against in-memory fakes for
the tests. The front-end is split the same way: `App` takes a keypress and
returns a `Task`; the controller performs it and answers with `Update`s.

Run the checks the way CI does:

```sh
cargo test --workspace     # 560 tests
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

The tests use real git repositories, real child processes, real Unix sockets,
and a real HTTP server on a loopback port, so the adapters are exercised
rather than mocked. Nothing in the suite needs network access or credentials.

## What is not implemented

Stated plainly, since the configuration mentions some of it:

- **GitLab, Discord, and Outlook** sources parse from configuration but have
  no adapter yet. `vetinari doctor` reports them; the rest of the inbox still
  works.
- **Converting a message into a ticket** (`<Space> c t`) is refused with a
  message rather than half-done - creating an issue needs a write API Vetinari
  does not have yet.
- **Herdr's IPC protocol** is Vetinari's assumption, documented at the top of
  `workspace/herdr.rs`: newline-delimited JSON with `create_space`,
  `focus_space`, `close_space`, `list_spaces`, and `run_in_space`. If Herdr
  speaks something else, that module is the only thing that needs to change.
- **`agy`'s command line** is assumed to take the prompt as its last argument.
  Adjust `args` in `[agents.agy-gemini]` if it does not.
- **No pseudo-terminal.** The PRD lists `portable-pty`; Vetinari runs managed
  agents with piped stdio in print mode instead, and hands the *interactive*
  command line to tmux or Herdr (`<Space> w r`) when you want to sit beside
  the agent. A pty already exists in those targets, so emulating a second one
  would add a terminal to debug rather than a feature. The cost is that an
  agent's own full-screen interface is not drawn inside Vetinari.
- **Sources fetch one page** (50 items, 25 events). There is no pagination.
- **A machine with no keychain service** - a headless container, say - reports
  `Keychain: unavailable on this machine` and no credential can be stored.

## Licence

MIT.
