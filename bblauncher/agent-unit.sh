#!/bin/bash
# systemd entry for bblauncher-agent@<name>-<port>
i="$1"
exec "$(dirname -- "${BASH_SOURCE[0]}")/agent-supervisor.sh" "${i%-*}" "${i##*-}"
