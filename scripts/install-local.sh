#!/bin/sh
# Build a release binary and install it as `jucode` for this user
# (~/.local/bin by default; override with PREFIX). Desktop finds it on PATH.
set -e
cd "$(dirname "$0")/.."
cargo build --release
dest="${PREFIX:-$HOME/.local}/bin"
mkdir -p "$dest"
install -m 0755 target/release/jucode "$dest/jucode"
echo "installed $("$dest/jucode" --version) at $dest/jucode"
