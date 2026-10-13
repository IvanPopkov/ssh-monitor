#!/usr/bin/env bash
set -euo pipefail
root="$(git rev-parse --show-toplevel)"
ln -sf "$root/scripts/pre-commit" "$root/.git/hooks/pre-commit"
echo "pre-commit hook installed -> scripts/pre-commit"
