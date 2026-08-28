#!/usr/bin/env bash
# Pass: hello.txt exists and contains exactly "hello world".
set -u
[ -f hello.txt ] || exit 1
[ "$(cat hello.txt)" = "hello world" ]
