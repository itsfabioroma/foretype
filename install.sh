#!/bin/sh
# Builds foretype from this checkout and installs it for every new terminal.
set -eu
cd "$(dirname "$0")"
command -v cargo >/dev/null 2>&1 || {
    echo "foretype: needs Rust (https://rustup.rs)" >&2
    exit 1
}
cargo build --release --locked
sudo ./target/release/foretype install
