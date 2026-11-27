#!/usr/bin/env bash
# Render Blockberg (mockup data) invisibly at 1672x941 and compare with tools/reference/mockup.png.
#   tools/compare.sh [outdir]   -> outdir/{shot,overlay,diff}.png + per-region scores
set -eu
cd "$(dirname "$0")/.."
out="${1:-${BLOCKBERG_COMPARE_DIR:-/tmp/blockberg-compare}}"; mkdir -p "$out"
ref=tools/reference/mockup.png
cargo build --release --bin blockberg-gui -q
xvfb-run -a -s "-screen 0 1672x941x24" bash -c "./target/release/blockberg-gui mockup & sleep ${WAIT:-4}; import -window root '$out/shot.png'; kill %1"
composite -blend 50 "$ref" "$out/shot.png" "$out/overlay.png"
compare -fuzz 12% "$ref" "$out/shot.png" "$out/diff.png" 2>/dev/null || true
printf '%-10s %s\n' region diff%
while read -r name geom; do
  n=$(compare -metric AE -fuzz 12% <(convert "$ref" -crop "$geom" +repage png:-) <(convert "$out/shot.png" -crop "$geom" +repage png:-) null: 2>&1 || true)
  w=${geom%%x*}; rest=${geom#*x}; h=${rest%%+*}
  awk -v n="${n%% *}" -v a=$((w*h)) -v r="$name" 'BEGIN{printf "%-10s %5.1f\n", r, 100*n/a}'
done <<'R'
header 1672x46+0+0
status 1672x44+0+46
nav 1672x38+0+90
watch 510x590+0+130
quote 805x110+520+130
chart 805x230+520+240
depth 805x245+520+475
ticket 340x590+1332+130
blotter 1672x165+0+722
ticker 1672x54+0+887
R
