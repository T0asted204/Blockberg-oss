#!/bin/bash
set -euo pipefail
project_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
export PATH="$HOME/.cargo/bin:$PATH"
cd "$project_dir"
log_dir="${XDG_STATE_HOME:-$HOME/.local/state}/blockberg"
mkdir -p "$log_dir"
bin="$project_dir/target/release/blockberg-gui"
# start at once when nothing changed since the last build; only rebuild when a source file is newer
if [ -x "$bin" ] \
   && [ -z "$(find src assets Cargo.toml Cargo.lock -newer "$bin" -type f -print -quit 2>/dev/null)" ]; then
    exec "$bin" "$@"
fi
if ! cargo build --release --bin blockberg-gui >"$log_dir/build.log" 2>&1; then
    command -v notify-send >/dev/null && notify-send "Blockberg build failed" "See $log_dir/build.log" || true
    cat "$log_dir/build.log" >&2
    exit 1
fi
exec "$project_dir/target/release/blockberg-gui" "$@"
