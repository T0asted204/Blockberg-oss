#!/bin/bash
# Installs the agent supervisor as systemd *user* units (paths filled in for this checkout). Does not start anything.
#   ./install-agent.sh   then: systemctl --user enable --now bblauncher-agent      (main account, port 7777)
#                         or:  systemctl --user enable --now bblauncher-agent@alt1-7787
set -euo pipefail
here="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
mkdir -p "$HOME/.config/systemd/user"
for u in bblauncher-agent.service bblauncher-agent@.service; do
  sed "s|@BBLAUNCHER_DIR@|$here|g" "$here/$u" > "$HOME/.config/systemd/user/$u"
done
systemctl --user daemon-reload
echo "installed. stop with: systemctl --user disable --now bblauncher-agent"
