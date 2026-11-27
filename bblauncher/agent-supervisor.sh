#!/bin/bash
# Keeps one Blockberg agent alive. Usage: agent-supervisor.sh [name] [port]   (default: main 7777)
# Starts the headless client (run-agent.sh). If the client exits, or the agent port stays closed for 30 s (a normal server hop
# takes under 25 s), it is killed and started again after 20 s. Hypixel kicks "rejoining too fast" if we come back within ~15 s.
here="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
NAME="${1:-main}"
PORT="${2:-7777}"
LOG_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/bblauncher"; mkdir -p "$LOG_DIR"
LOG="$LOG_DIR/agent-run-$NAME.log"
kill_game() { for p in $(pgrep -f "[b]blauncher/java/25/bin/jav[a].*instances/$NAME/"); do kill "$p"; done; }
while true; do
  "$here/run-agent.sh" "$NAME" "$PORT" >> "$LOG" 2>&1 &
  PID=$!
  down_since=$(date +%s)
  grace=$(( $(date +%s) + 90 ))           # the first start needs time to load and join (about 30 s; a join that hangs at "Connecting" is retried)
  while kill -0 "$PID" 2>/dev/null; do
    sleep 3
    if ss -ltn 2>/dev/null | grep -q ":$PORT "; then down_since=$(date +%s); grace=0
    elif [ "$(date +%s)" -gt "$grace" ] && [ $(( $(date +%s) - down_since )) -gt 30 ]; then
      echo "[supervisor $(date +%T)] agent port closed for 30 s: restarting the client" >> "$LOG"
      kill "$PID" 2>/dev/null
      kill_game
      break
    fi
  done
  wait "$PID" 2>/dev/null
  echo "[supervisor $(date +%T)] client stopped, restarting in 20 s" >> "$LOG"
  sleep 20
done
