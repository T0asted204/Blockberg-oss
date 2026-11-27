#!/bin/bash
# Installs the Blockberg server as a systemd *user* service. It does not start it unless you pass --now.
#   tools/install-server.sh          install + enable (starts at your next login)
#   tools/install-server.sh --now    ... and start it right away
set -euo pipefail
here="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
mkdir -p "$HOME/.config/systemd/user"
sed "s|@BLOCKBERG_DIR@|$(dirname "$here")|g" "$here/blockberg-server.service" > "$HOME/.config/systemd/user/blockberg-server.service"
systemctl --user daemon-reload
systemctl --user enable blockberg-server.service
# keeps the user's services running when you are logged out (optional; needs permission)
loginctl enable-linger "$USER" 2>/dev/null || echo "note: could not enable lingering; the server runs while you are logged in"
if [ "${1:-}" = "--now" ]; then systemctl --user start blockberg-server.service; fi
echo "secret file: $HOME/.config/blockberg/server_secret (created on the first start)"
echo "logs:        journalctl --user -u blockberg-server -f"
