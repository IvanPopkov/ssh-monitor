#!/usr/bin/env bash
# Refused cases never reach the real ssh, so nothing connects.
set -uo pipefail
G="$(cd "$(dirname "$0")" && pwd)"; fail=0
export GUARD_LOG; GUARD_LOG="$(mktemp -t guard-test)"; trap 'rm -f "$GUARD_LOG"' EXIT
expect() {
  local want=$1; shift
  PATH="$G:$PATH" ssh "$@" >/dev/null 2>&1; local got=$?
  if [[ "$want" == refused && $got -eq 1 ]] || [[ "$want" == allowed && $got -ne 1 ]]; then echo "ok   $want: ssh $*"
  else echo "FAIL $want (exit $got): ssh $*"; fail=1; fi
}
before=0
expect refused fedora
expect refused fedora true
expect refused -t fedora -- dev-session 1
expect refused -O exit fedora
expect refused -O stop fedora
expect refused -o ControlPath=/x -O check fedora
expect refused -O check -oProxyCommand=x fedora
expect refused -N -o ControlMaster=yes fedora
expect refused -G
expect allowed -G fedora
expect allowed -V
expect allowed -O check fedora   # real master check (exit 0 up / 255 down — both "allowed")
after=$(wc -l <"$GUARD_LOG")
[[ $((after - before)) -eq 9 ]] && echo "ok   9 refusals logged" || { echo "FAIL expected 9 new log lines, got $((after - before))"; fail=1; }
[[ "$(PATH="$G:$PATH" command -v ssh)" == "$G/ssh" ]] && echo "ok   guard/ssh shadows real ssh on PATH" || { echo "FAIL PATH order"; fail=1; }
exit $fail
