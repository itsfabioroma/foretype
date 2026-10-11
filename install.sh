#!/bin/sh
# Builds foretype from this checkout and installs it as `ssh` ahead of the system ssh.
#   ./install.sh          system-wide, /usr/local/bin (asks for sudo)
#   ./install.sh --user   this account only, ~/.local/bin
set -eu
cd "$(dirname "$0")"
command -v cargo >/dev/null 2>&1 || {
    echo "foretype: needs Rust (https://rustup.rs)" >&2
    exit 1
}
cargo build --release --locked
if [ "${1:-}" = "--user" ]; then
    ./target/release/foretype install --user
else
    sudo ./target/release/foretype install
fi
