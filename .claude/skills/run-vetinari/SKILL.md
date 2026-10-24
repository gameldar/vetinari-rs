---
name: run-vetinari
description: Build, run, and drive the vetinari TUI. Use when asked to start vetinari, run it, try the attention queue or its keybindings in the real app, capture its screen, or confirm a change works beyond the test suite.
---

Vetinari is a ratatui terminal app. Drive it with
`.claude/skills/run-vetinari/driver.sh`, which runs it in tmux against
`fakegh.py`, a local stand-in for GitHub's GraphQL search, so the queue has
PRs you control. All paths are relative to the repo root.

## Prerequisites

`cargo`, `tmux`, `python3` and `curl` on PATH, plus a working system keyring
(vetinari only stores credentials there; the driver writes a dummy GitHub
token under its own `drive` account, leaving real accounts alone).

## Run (agent path)

```bash
.claude/skills/run-vetinari/driver.sh start       # builds, starts fakegh + vetinari, prints the queue
.claude/skills/run-vetinari/driver.sh keys "SPC n" "SPC n"
.claude/skills/run-vetinari/driver.sh queue       # left pane as plain text
.claude/skills/run-vetinari/driver.sh details     # details pane for the selected row
.claude/skills/run-vetinari/driver.sh stop
```

| command | what it does |
|---|---|
| `start` | `cargo build`, launches `fakegh.py` on 127.0.0.1:8765 and vetinari (200x50) with `drive.toml`, waits for the first PR |
| `keys "SPC d p c" j ...` | sends keys in vetinari's spelling (`SPC`, `ESC`, `RET`, `TAB`, `C-c`, letters) |
| `queue` / `details` | the queue pane / details pane, box drawing stripped |
| `screen` | the whole screen |
| `status` | the status line (fades after a few seconds - read it straight after the key) |
| `wait TEXT [SECS]` | blocks until TEXT is on screen, dumps the screen on timeout |
| `prs FILE` | replaces the PRs fakegh serves; seen on the next refresh (every 2s) |
| `stop` | quits vetinari and the fake |

Changing what the source reports, e.g. to see the queue react when a PR
disappears:

```bash
python3 -I -c "import json,sys;json.dump([p for p in json.load(open(sys.argv[1])) if p['n']!=103],open(sys.argv[2],'w'))" .claude/skills/run-vetinari/prs.json /tmp/no103.json
.claude/skills/run-vetinari/driver.sh prs /tmp/no103.json
.claude/skills/run-vetinari/driver.sh wait "gone from github" 10
```

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
- **Focus time is shown in the machine's local time** ("Focus until 16:47"),
  not UTC.
- **There is no in-app log view.** Resurfacing is logged as
  `INFO queue: ... came back (Rule)` in the feedback log only; on screen,
  watch the row note ("back: it changed") and `status`.

## Troubleshooting

- **`timed out waiting for: Fix login redirect` with "github is unavailable"
  on the status line**: fakegh is not running (`tmux ls` has no
  `vetdrive-fake`). Run `driver.sh stop`, then `start` again.
