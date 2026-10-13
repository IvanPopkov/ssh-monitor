#!/usr/bin/env bash
# The only way to run against the live host: guard/ssh first on PATH blocks any non-read-only ssh.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
cargo build -q -p ssh-monitor --manifest-path "$root/Cargo.toml"
PATH="$root/guard:$PATH" exec "$root/target/debug/ssh-monitor" "$@"
