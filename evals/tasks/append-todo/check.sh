#!/usr/bin/env bash
# Pass: the new task line was appended and the existing lines survived.
set -u
[ -f NOTES.md ] || exit 1
grep -qF -- "- [ ] write tests" NOTES.md || exit 1
grep -qF -- "- [x] set up repository" NOTES.md || exit 1
grep -qF -- "- [ ] draft the design" NOTES.md
