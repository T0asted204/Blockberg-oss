#!/bin/bash
# Prepares a second (third, ...) Blockberg account: its Minecraft instance, ports and profile folders.
# Usage: add-agent.sh <name> <index>     e.g. add-agent.sh alt1 1   (index 1 -> agent port 7787, server 7782/7783)
# Does NOT log in or start anything: after it, run `bblauncher login <name>` yourself, then see the printed steps.
set -euo pipefail
NAME="${1:?name, e.g. alt1}"; IDX="${2:?index, 1 for the first extra account}"
[[ "$NAME" =~ ^[a-z][a-z0-9_-]*$ && "$NAME" != main ]] || { echo "name: lowercase letters/digits, not 'main'"; exit 1; }
AGENT=$((7777 + 10*IDX)); SRV=$((7780 + 2*IDX)); LOCK=$((SRV + 1))
ZL="$HOME/.local/share/bblauncher/instances"
if [ -e "$ZL/$NAME" ]; then echo "instance $NAME already exists"; else
  mkdir -p "$ZL/$NAME"
  cp -a "$ZL/main/instance.json" "$ZL/main/natives" "$ZL/$NAME/"
  mkdir -p "$ZL/$NAME/game"
  (cd "$ZL/main/game" && cp -a config data mods minescript options.txt quickPlay resourcepacks servers.dat "$ZL/$NAME/game/" 2>/dev/null || true)
  rm -rf "$ZL/$NAME/game/minescript/__pycache__" "$ZL/$NAME"/game/minescript/*.before-*
  sed -i "s/\"name\": \"main\"/\"name\": \"$NAME\"/" "$ZL/$NAME/instance.json"
  echo "instance $NAME created (copy of main)"
fi
python3 - "$NAME" "$AGENT" "$SRV" "$LOCK" <<'P'
import json,os,sys
name,agent,srv,lock=sys.argv[1:]
p=os.path.expanduser("~/.config/blockberg/agents.json")
d=json.load(open(p)) if os.path.exists(p) else {"agents":[]}
d["agents"]=[a for a in d["agents"] if a["name"]!=name]+[{"name":name,"server":f"127.0.0.1:{srv}","lock":f"127.0.0.1:{lock}","agent":f"127.0.0.1:{agent}"}]
json.dump(d,open(p,"w"),indent=1)
P
echo "{\"agent\": $AGENT, \"fast\": $((AGENT+2))}" > "$ZL/$NAME/game/minescript/bb_ports.json"
mkdir -p "$HOME/.local/share/blockberg/profiles/$NAME" "$HOME/.config/blockberg/profiles/$NAME"
cat <<T
Ports for $NAME: agent $AGENT (in-game helper $((AGENT+2))), server 127.0.0.1:$SRV, lock $LOCK.
Next steps:
  1. ~/bblauncher/target/release/bblauncher login $NAME        (sign in with the $NAME Minecraft account)
  2. put this account's UUID in ~/.config/blockberg/profiles/$NAME/hypixel_uuid  (for the bank / sack API; the API key is shared)
  3. systemctl --user enable --now bblauncher-agent@$NAME-$AGENT blockberg-server@$NAME
  4. open the window:  BLOCKBERG_PROFILE=$NAME ~/blockberg/tools/launch-gui.sh
T
