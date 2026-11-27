#!/bin/bash
# Runs the real client on a hidden virtual display (software GL) and saves a screenshot every 3 s, for debugging.
# Usage: xvfb-run.sh [instance] [account] [outdir]     env BB_SERVER (default hypixel.net)
here="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
INST="${1:-main}"; ACC="${2:-$INST}"; OUT="${3:-./xvfb-shots}"; mkdir -p "$OUT"
export LIBGL_ALWAYS_SOFTWARE=1 BBL="$here/target/release/bblauncher" INST ACC OUT SRV="${BB_SERVER:-hypixel.net}"
exec xvfb-run -a -s "-screen 0 1280x720x24" bash -c '
  "$BBL" launch "$INST" "$ACC" --server "$SRV" --after-join "/lobby;;/play skyblock" > "$OUT/client.log" 2>&1 &
  for i in $(seq 1 100); do sleep 3; import -window root "$OUT/shot_$(printf %03d $i).png" 2>/dev/null; done'
