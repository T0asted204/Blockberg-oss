#!/bin/bash
# One headless Blockberg agent: joins the server and goes to SkyBlock.
# Usage: run-agent.sh [instance/account name] [agent port]     env BB_SERVER (default hypixel.net)
here="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
NAME="${1:-main}"; PORT="${2:-7777}"
exec "${BBLAUNCHER_BIN:-$here/target/release/bblauncher}" launch "$NAME" "$NAME" --headless --server "${BB_SERVER:-hypixel.net}" --skyblock --agent-port "$PORT"
