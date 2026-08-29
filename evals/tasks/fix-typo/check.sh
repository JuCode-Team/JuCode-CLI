#!/usr/bin/env bash
# Pass: greeting.py now prints exactly "hello world" (checked by running it
# when python3 is available, by content otherwise).
set -u
[ -f greeting.py ] || exit 1
if command -v python3 >/dev/null 2>&1; then
    [ "$(python3 greeting.py)" = "hello world" ]
else
    grep -q '"hello world"' greeting.py && ! grep -q '"helo world"' greeting.py
fi
