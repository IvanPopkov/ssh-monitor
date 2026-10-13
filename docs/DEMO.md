# Demo — see it work

Everything below is safe for the live connection: the binary only runs `ssh -G` and `ssh -O check`,
and `scripts/live.sh` puts `guard/ssh` first on `PATH` so nothing else can reach ssh. Any step that
would change the connection is marked **you do it**, and the monitor only watches.

## 1. The live TUI

```sh
scripts/live.sh                 # builds, then runs against the live master (host `fedora`)
scripts/live.sh --host <other-alias>   # any ssh alias that has a ControlPath configured
```
What you should see within ~3 s (16 rows minimum, more rows = taller charts):
- **Status bar**: `● UP  fedora  up 4h12m  for 3s` and the clock on the right. `for X` is the time in the
  current state. Badges appear when they apply: `⚠ STORM` (red), `⚡ BREAKER` (red), `↻ RETRY` (yellow).
  `⚠ STORM` shows only while an `autossh` or a direct (non-mux) `ssh` to the host is running.
- **rx / tx**: received / sent bytes per second; two charts of Braille columns, 5-minute history (below 30 rows: one mirrored panel, rx up and tx down from a shared zero line, titled `↓ rx … ↑ tx …` — the arrows are the data direction: `↓` download from the host, `↑` upload to it), fixed log axis 100 B/s … 100 MB/s. Titles show the
  current rate and the 5-min peak. Run something on the host (`ls -R /usr` in a session) and watch rx jump.
- **latency · direct · LAN**: now/jitter/loss and min/avg/max over 5 minutes, plus a sparkline.
- **clients (N)**: `#1 … #4` (dev-sessions), `vscode`, others by their remote command, each with its age;
  direct clients in red with `⚠`; `+k more` when they do not fit.
- **events**: newest first, coloured by severity. At start: `master up (pid …)`, `clients +#1 +#2 …`.

Keys: `p` freezes the display (a `PAUSED` badge and a frozen clock; sampling/CSV go on), `c` clears the
event log, `q` quits. Resize the window: below 64×16 it says `terminal too small`.

## 2. One-shot JSON and exit codes

```sh
scripts/live.sh --once | jq .          # ~3 s sample, JSON, exit 0 UP / 1 DEGRADED / 2 DOWN / 3 error
scripts/live.sh --once; echo "exit=$?"
scripts/live.sh --once | jq '{state, pid, rx_bps, tx_bps, rtt_ms, loss, path, clients, badges, events}'
```
`rx_bps: null` means "not measured" (for instance when `nettop` took ~5 s), never zero.

## 3. CSV log and recording

```sh
scripts/live.sh --log /tmp/sshmon.csv            # one row every 2 s (the default --interval); q to stop
head -3 /tmp/sshmon.csv                          # ts,state,pid,rx_bps,tx_bps,rtt_ms,loss,path,clients,ssh_procs,events
scripts/live.sh --once --record /tmp/sshmon-rec  # every probe command + output → /tmp/sshmon-rec/tape.jsonl
head -2 /tmp/sshmon-rec/tape.jsonl
```
A recorded tape replays in the test layers by time (`select_entry` in `crates/testkit`); the L2 test
`l2_record_writes_a_tape_that_replays_to_the_same_result` does exactly that with a fake recording.

## 4. Triggering events on the live connection (you do it, in another terminal)

| Feature | You do | Monitor shows |
|---|---|---|
| clients `+…` / `−…` | `ssh fedora` in a new terminal, then exit | `clients +ssh`, then `clients −ssh`; count changes |
| direct client `⚠` + storm (b) | `ssh -o ControlMaster=no -o ControlPath=none fedora` | red `⚠ ssh` in clients, `storm: non-mux ssh pid …`, `⚠ STORM`; on exit `storm cleared after …` |
| storm (a) | start `autossh -M 0 -N fedora` (if installed), stop it | `storm: autossh present (pid …)` / cleared |
| DOWN → restart | `ssh -O exit fedora` — **warning: this also closes every session multiplexed through the master (#1–#4, VS Code)**; the supervisor restarts it | `○ DOWN  down 3s (was up 4h12m)`, `master down after Ns (rc=N)` (merged from the supervisor log), the supervisor's `reconnecting in Ns` line, then `master restarted (pid …)`; rx/tx charts show a gap. Supervisor lines written before the 2026-10-13 supervisor fix; re-check them on the next real restart |
| DEGRADED (loss) | turn Wi-Fi/Tailscale off for ~15 s | `◐ DEGRADED (loss)`, `degraded: 3 lost pings`, blanks in the sparkline, then `recovered after 20s` |
| path changed | switch between LAN and a WAN/DERP path (e.g. tethering) | title `direct · WAN` / `DERP par`; `path changed: …` only for direct ↔ DERP |
| system resumed | close the lid for a minute | `system resumed after 1m02s` |

## 5. Every feature without touching anything: rendered frames from the test suite

The L1 tests drive the real loop with fake tools on a paused clock and render into a test terminal.
Dump the last frame of each test:

```sh
SSHMON_DUMP_FRAMES=1 cargo test -p ssh-monitor --test l1_tui -- --nocapture --test-threads 1
```
You will see, among others:
- `down_view_shows_no_master_rogues_and_a_gap_in_the_charts` — `○ DOWN … (was up 10s)`, `clients · no master`, a red `⚠ ssh`, the chart history ending in a gap;
- `scenario_timeline_up_loss_degraded_recovered_down_loop_up` — a 120×40 frame after
  UP → lost pings → DEGRADED → recovered → DOWN → 3-attempt reconnect loop (`↻ RETRY`) → `CIRCUIT OPEN`
  (`⚡ BREAKER`) → new master, with the event log newest-first;
- `status_bar_keeps_paused_and_clock_when_narrow` — all badges + `PAUSED` at 64 columns;
- `client_overflow_keeps_direct_clients_visible` — `+k more` with the `⚠` clients kept visible.

The same scenarios run through the real binary with `fake-cmd` on `PATH`:

```sh
cargo test -p ssh-monitor --test l2_once -- --nocapture     # --once UP/DEGRADED/DOWN/STORM, --log, --record, missing tool
```

## 6. Checks

```sh
scripts/check.sh --all    # fmt, clippy -D warnings, unit + L1 + L2 (≈20 s)
guard/test.sh             # the ssh shim refuses everything but -O check / -G / -V
ls guard/guard.log        # must not exist (the binary never tried a forbidden ssh form)
```
