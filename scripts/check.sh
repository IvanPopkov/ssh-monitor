#!/usr/bin/env bash
# Quality gate shared by pre-commit and CI. --all adds L2 tests.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "==> fmt";    cargo fmt --all -- --check
echo "==> clippy"; cargo clippy --workspace --all-targets -- -D warnings
if [[ "${1:-}" == "--all" ]]; then
  echo "==> tests (unit + L1 + L2)"; cargo test --workspace
else
  echo "==> tests (unit + L1)"; cargo test --workspace -- --skip l2_
fi
echo "==> OK"
