---
name: run-vetinari
description: Build, run, and drive the vetinari TUI. Use when asked to start vetinari, run it, try the attention queue or its keybindings in the real app, capture its screen, or confirm a change works beyond the test suite.
---

Vetinari is a ratatui terminal app. Drive it with
`.claude/skills/run-vetinari/driver.sh`, which runs it in tmux against
`fakegh.py`, a local stand-in for GitHub's GraphQL search, so the queue has
PRs you control, and a `vetinarid --scripted` replaying recorded dev
workflow runs, so the queue has runs with phase strips and a gate to answer.
All paths are relative to the repo root.

## Prerequisites

`cargo`, `tmux`, `python3` and `curl` on PATH, plus a working system keyring
(vetinari only stores credentials there; the driver writes a dummy GitHub
token under its own `drive` account, leaving real accounts alone).

## Run (agent path)

```bash
.claude/skills/run-vetinari/driver.sh start       # builds, starts fakegh + vetinari, prints the queue
.claude/skills/run-vetinari/driver.sh keys "SPC n" "SPC n"
.claude/skills/run-vetinari/driver.sh queue       # the queue and recent actions as plain text
.claude/skills/run-vetinari/driver.sh details     # the selected row's details popup
.claude/skills/run-vetinari/driver.sh stop
```

| command | what it does |
|---|---|
| `start` | `cargo build`, launches `fakegh.py` on 127.0.0.1:8765, a scripted vetinarid on `$STATE/vetinarid.sock` and vetinari (80x24, or `VETINARI_DRIVE_SIZE=60x12` for a zax-panel-sized one) with `drive.toml` and `--daemon` at that socket, waits for the first PR |
| `keys "SPC d p c" j ...` | sends keys in vetinari's spelling (`SPC`, `ESC`, `RET`, `TAB`, `C-c`, letters) |
| `queue` | the pane minus its bottom line, box drawing stripped |
| `details` | presses `RET`, prints the details popup, presses `ESC` |
| `screen` | the whole screen |
| `status` | the bottom line: a status message while one shows (they fade after a few seconds - read it straight after the key), the key hints otherwise |
| `wait TEXT [SECS]` | blocks until TEXT is on screen, dumps the screen on timeout |
| `prs FILE` | replaces the PRs fakegh serves; seen on the next refresh (every 2s) |
| `stop` | quits vetinari and the fake |

Changing what the source reports, e.g. to see the queue react when the PR
being worked on disappears (#101's failing CI always ranks it first, so the
first `SPC n` takes it):

```bash
.claude/skills/run-vetinari/driver.sh keys "SPC n"
python3 -I -c "import json,sys;json.dump([p for p in json.load(open(sys.argv[1])) if p['n']!=101],open(sys.argv[2],'w'))" .claude/skills/run-vetinari/prs.json /tmp/no101.json
.claude/skills/run-vetinari/driver.sh prs /tmp/no101.json
.claude/skills/run-vetinari/driver.sh wait "gone from github" 10
```

### Dev workflow runs

`start` replays `crates/vetinari-daemon/tests/fixtures/gate-run.script.json`
by default: ENG-398 waits at its info-needed gate (`[run] ENG-398 Paginate
audit export  pre? impl· rev· pr·` at the top of NEXT) and ENG-412 works
through its rounds on its own. Set `VETINARI_DRIVE_RUNS` to another script,
or to `none` to start with no daemon. The daemon's log is
`$STATE/vetinarid.log`.

```bash
.claude/skills/run-vetinari/driver.sh keys "g g" RET   # the run's popup: rows, rounds, questions, gate keys
.claude/skills/run-vetinari/driver.sh keys r           # recheck: status says "sent", preflight goes to round 2
.claude/skills/run-vetinari/driver.sh keys Left        # look at round 1 of the intake loop
.claude/skills/run-vetinari/driver.sh keys x           # abandon the run; the popup closes, the row resolves
```

To run the daemon by hand against the TUI:

```bash
cargo run --bin vetinarid -- --scripted crates/vetinari-daemon/tests/fixtures/gate-run.script.json --socket /tmp/v.sock
cargo run --bin vetinari -- --daemon /tmp/v.sock
```

Without `--daemon` both sides use `$XDG_RUNTIME_DIR/vetinari/vetinarid.sock`.
A TUI that never reaches a daemon queues no runs and says nothing; one that
loses its daemon shows "vetinarid is unavailable" until it comes back.

`prs.json` documents the entry fields in `fakegh.py`'s docstring: bump
`updated` to make an item count as changed, set `checks` to `FAILURE` for a
CI failure, drop an entry to make it gone.

## Run (human path)

```bash
cargo run --bin vetinari    # your own ~/.config/vetinari config; SPC h k lists keys, SPC q quits
```

## Test

```bash
cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check
```

## Gotchas

- **tmux resolves `-t name` by prefix.** With sessions `vet-drive` and
  `vet-drive-fakegh`, `kill-session -t vet-drive` kills the fake when the app
  session does not exist yet - vetinari then shows "github is unavailable:
  error sending request". The driver's session names are deliberately not
  prefixes of each other; keep it that way.
- **`cut -c` and mawk count bytes**, so slicing panes out of `capture-pane`
  splits the box-drawing characters and `grep` calls the output binary. The
  driver slices in Python.
- **Running against your real config shows no PRs if the stored GitHub token
  belongs to another user** - `gh` may list review requests that vetinari's
  `home` credential does not see. That is why the driver uses fakegh.
- **The order of the other PRs drifts with the real clock**, since how long a
  PR has waited counts towards its rank and `prs.json`'s times are fixed. Only
  #101 (CI failed) reliably comes first; pick rows by title, not position.
- **Focus time is shown in the machine's local time** ("focus until 16:47" in the title),
  not UTC.
- **The recent actions under the queue are the on-screen log** ("09:20 back,
  it changed: Add billing export"). In a 12-row panel the default five of them
  leave the queue four lines, and the list scrolls to keep the selected row in
  view, so a row can be off screen - select it (`G`, `j`) before asserting on
  it.
- **The details popup is clipped to the screen**, so at 60x12 its last fields
  do not show; use the default size when they matter.

## Troubleshooting

- **`timed out waiting for: Fix login redirect` with "github is unavailable"
  on the status line**: fakegh is not running (`tmux ls` has no
  `vetdrive-fake`). Run `driver.sh stop`, then `start` again.
