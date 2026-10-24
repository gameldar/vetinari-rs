#!/usr/bin/env bash
# Drive the vetinari TUI inside tmux against a fake GitHub. Run from the repo root.
# usage: driver.sh start | keys "SPC n" ... | screen | queue | details | status
#        | wait TEXT [SECS] | prs FILE | stop
set -euo pipefail

SKILL=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$SKILL/../../.." && pwd)
STATE=${VETINARI_DRIVE_STATE:-${TMPDIR:-/tmp}/vetinari-drive}
# Neither name may be a prefix of the other: tmux resolves -t by prefix.
APP=vetdrive-app
FAKE=vetdrive-fake
BIN=$ROOT/target/debug/vetinari
CONFIG=$SKILL/drive.toml

screen() { tmux capture-pane -t "$APP" -p; }

wait_for() {
    local text=$1 secs=${2:-15}
    timeout "$secs" bash -c "until tmux capture-pane -t $APP -p | grep -qF -- \"\$0\"; do sleep 0.2; done" "$text" \
        || { echo "timed out waiting for: $text" >&2; screen >&2; exit 1; }
}

# Slice columns by character; cut and mawk count bytes and split the box drawing.
cols() { python3 -I -c 'import sys
a, b = int(sys.argv[1]), int(sys.argv[2])
for line in sys.stdin:
    text = line.rstrip("\n")[a:b].strip("│┌┐└┘─ ")
    if text:
        print(text)' "$1" "$2"; }

# The queue is the left pane, the details the lower right.
queue() { screen | sed -n '1,47p' | cols 0 90; }

details() { screen | sed -n '/CONTEXT \/ DETAILS/,47p' | cols 90 200; }

# Line 49 is the status line; it fades after a few seconds.
status() { screen | sed -n 49p | sed -E 's/ +$//'; }

# Vetinari spells keys like Doom ("SPC d p h", "ESC", "RET"); tmux wants Space/Escape/Enter.
keys() {
    for spelling in "$@"; do
        for part in $spelling; do
            case $part in
                SPC) key=Space ;;
                ESC) key=Escape ;;
                RET) key=Enter ;;
                TAB) key=Tab ;;
                *) key=$part ;;
            esac
            tmux send-keys -t "$APP" "$key"
            sleep 0.1
        done
    done
    sleep 0.5
}

start() {
    mkdir -p "$STATE"
    cargo build -q --bin vetinari --manifest-path "$ROOT/Cargo.toml"
    cp "$SKILL/prs.json" "$STATE/prs.json"
    tmux kill-session -t "$FAKE" 2>/dev/null || true
    tmux new-session -d -s "$FAKE" "python3 -I '$SKILL/fakegh.py' '$STATE/prs.json' 8765"
    timeout 5 bash -c 'until curl -s -o /dev/null -X POST http://127.0.0.1:8765/graphql -d "{}"; do sleep 0.2; done'
    printf 'drive-fake-token\n' | "$BIN" auth github --config "$CONFIG" --account drive >/dev/null
    tmux kill-session -t "$APP" 2>/dev/null || true
    tmux new-session -d -s "$APP" -x 200 -y 50 "'$BIN' --config '$CONFIG'"
    wait_for "Fix login redirect" 20
    queue
}

stop() {
    tmux send-keys -t "$APP" Space q 2>/dev/null || true
    sleep 0.5
    tmux kill-session -t "$APP" 2>/dev/null || true
    tmux kill-session -t "$FAKE" 2>/dev/null || true
}

cmd=${1:-}
shift || true
case $cmd in
    start) start ;;
    keys) keys "$@" ;;
    screen) screen ;;
    queue) queue ;;
    details) details ;;
    status) status ;;
    wait) wait_for "$@" ;;
    prs) cp "$1" "$STATE/prs.json" ;;
    stop) stop ;;
    *) sed -n '2,4p' "$0" >&2; exit 2 ;;
esac
