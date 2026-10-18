#!/bin/sh
# A stand-in oh-my-pi for tests/acp_tui.rs: its version, its ACP agent (the
# fake one, $FAKE_ACP) or its TUI. The TUI prints its arguments, echoes each
# line typed and saves it in $FAKE_ACP_STATE as a turn, and exits on `quit`.
case "$1" in
--version) echo "omp/18.1.21"; exit 0 ;;
acp) shift; exec python3 "$FAKE_ACP" "$@" ;;
esac
echo "TUI omp $*"
while IFS= read -r line; do
  [ "$line" = quit ] && exit 0
  echo "tui: $line"
  python3 - "$line" <<'PY'
import json, os, sys
path = os.environ["FAKE_ACP_STATE"]
turns = json.load(open(path)) if os.path.exists(path) else []
json.dump(turns + [[sys.argv[1], "tui: " + sys.argv[1]]], open(path, "w"))
PY
done
