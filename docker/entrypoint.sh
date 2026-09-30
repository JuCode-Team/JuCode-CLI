#!/bin/sh
# First start seeds config.json:
# - the JuCode gateway and a model it serves (JUCODE_MODEL, default
#   gpt-6-sol); fields left out fall back to non-JuCode defaults;
# - the sandbox off: the container is the isolation boundary
#   (gVisor/Firecracker in production, see docs/cloud-agent.md), and the
#   engine's own sandbox needs user namespaces a container lacks.
set -e
mkdir -p "$HOME/.jucode"
if [ ! -f "$HOME/.jucode/config.json" ]; then
    printf '{"provider":"jucode","model":"%s","sandbox":"full-access"}\n' \
        "${JUCODE_MODEL:-gpt-6-sol}" > "$HOME/.jucode/config.json"
fi
# The relay is the only way in.
jucode daemon relay on >/dev/null
exec jucode daemon --listen 127.0.0.1:7788 "$@"
