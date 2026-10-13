# ssh-monitor

A btop-style terminal monitor for **one SSH ControlMaster connection** on macOS. It watches the
master socket of a host (`fedora` by default), its throughput, latency and Tailscale path, the
clients multiplexed through it, and the `ssh-master` supervisor log — and it never touches the
connection: the only ssh commands it ever runs are `ssh -G <host>` and `ssh -O check <host>`.

```
● UP  ⚠ STORM  fedora  up 1h01m  for 1h01m              01:01:40
── ↓ rx 1.0 kB/s · peak 1.0 kB/s   ↑ tx 500 B/s · peak 500 B/s ─

⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤
⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛

── latency · direct · LAN ───────── ── clients (8) ─────────────
now 10ms  jitter 0.0ms  loss 0.0%   ⚠ vscode 1h01m  +5 more
min 10ms  avg 10ms  max 10ms        ⚠ ssh 1h01m
███████████████████████████████████ #1 1h01m
── events ──────────────────────────────────────────────────────
00:00:00 storm: non-mux ssh pid 2011 (ssh)
00:00:00 storm: non-mux ssh pid 2010 (ssh)
00:00:00 clients +#1 +#2 +#3 +#4 +#5 +#6 +vscode +ssh
00:00:00 master up (pid 1000)
```
(a 64×16 frame rendered by the test suite against fake tools: below 30 rows rx grows up and tx
grows down from a shared zero line; `↓`/`↑` = data direction)

Spec: [docs/SPEC.md](docs/SPEC.md). Design: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).
How to see each feature: [docs/DEMO.md](docs/DEMO.md).

## Install

```sh
cargo install --locked --path crates/app   # installs the `ssh-monitor` binary (uses the checked-in Cargo.lock)
```

Requirements: macOS (uses `nettop`, `lsof`, `ps`, `ping`; `jq` is handy for the JSON), OpenSSH with `ControlMaster`/`ControlPath`
configured for the host, optionally `tailscale` on `PATH` and the `ssh-master` supervisor log at
`~/.local/state/ssh-master-<host>.log` (missing → those panels show `n/a`).

## Usage

```
ssh-monitor [--host <alias>] [--interval <secs>] [--log <csv>] [--once] [--record <dir>]
```

| Flag | Meaning |
|---|---|
| `--host <alias>` | ssh host alias to watch (default `fedora`). Resolved once via `ssh -G`; no `ControlPath` → exit 3 |
| `--interval <secs>` | base tick, default 2 (`--once`: 1), min 0.5, max 3600. 1×: traffic, ping, CSV · 2×: status, clients, lsof, uptime, supervisor log · 10×: tailscale. Render is fixed at 250 ms |
| `--log <path>` | append one CSV row per tick: `ts,state,pid,rx_bps,tx_bps,rtt_ms,loss,path,clients,ssh_procs,events` |
| `--once` | sample ~3 s, print a JSON snapshot, exit `0`/`1`/`2` = UP/DEGRADED/DOWN (`3` = error, including "status never answered") |
| `--record <dir>` | save every probe command and its output to `<dir>/tape.jsonl` (replayable by the tests) |

Keys: `q` quit · `p` freeze the display (sampling, events and CSV continue) · `c` clear the on-screen log.
Minimum terminal size 64×16; extra rows go to the charts and the event log. Below 30 rows rx and tx share one mirrored panel (rx up, tx down).

## How each metric is measured

| What | Command | Rule |
|---|---|---|
| **State** `● UP` / `◐ DEGRADED` / `○ DOWN` | `ssh -O check <host>` every 2 ticks | DOWN = no master. DEGRADED = master alive and any of: ≥3 consecutive lost pings · 5 s median RTT > max(3× baseline, baseline + 100 ms) · no bytes received for > 2× `ServerAliveInterval` + 5 s. Back to UP after 5 s of clean pings. Before the first answer the state is `n/a` |
| **Master uptime** | `ps -o etime= -p <pid>` | `up X`; after a drop `down X (was up Y)` |
| **rx / tx** charts | `nettop -p <pid> -P -L 1 -x -J bytes_in,bytes_out` every tick | counter differences over monotonic time, drawn as Braille columns covering the span each difference measured; 5-min history on a fixed log axis 100 B/s … 100 MB/s; gaps while DOWN or when nettop fails or has no row |
| **Latency** now · min/avg/max · jitter · loss | `ping -c 1 -t 1 <ip>` every tick (also while DOWN) | all over the last 5 min; jitter = mean \|ΔRTT\|; a timed-out ping is lost. Baseline = 5-min median of pings taken while UP (needs 10), reset on path change |
| **Path** in the latency title | `tailscale status --json` every 10 ticks, peer matched by IP | `direct · LAN` / `direct · WAN` (`CurAddr` set, private or not) / `DERP <region>` / `relay <via>` (Tailscale peer relay) / `n/a` (peer offline or probe failed) |
| **Clients** | `ps -axo pid=,ppid=,etime=,command=` every 2 ticks | `ssh` processes targeting the host (alias or IP), master excluded. `dev-session N` → `#N`, VS Code Remote-SSH → `vscode`, otherwise the remote argv (`scp`, `rsync`, …) or `ssh` |
| **Direct client `⚠`** / **storm** | `lsof -n -P -iTCP@<ip>:22` every 2 ticks | a PID other than the master with its own TCP connection (a different socket than the master's) is direct; each one is a storm culprit, as is any `autossh` process |
| **Supervisor** badges & events | `tail -n 50 ~/.local/state/ssh-master-<host>.log` every 2 ticks | `reconnecting in Ns` (dim), `CIRCUIT OPEN` (red, `⚡ BREAKER` badge), `CIRCUIT HALF-OPEN`; `master ended after Ns (rc=N)` within ±5 s is merged into `master down`. **Reconnect loop** (`↻ RETRY`): ≥3 consecutive attempts during which the master was never seen running, whatever the log calls them |
| **system resumed** | wall clock vs monotonic clock | the wall clock jumped ≥10 s past the monotonic one: the laptop slept; the gap holds no data |

Anything that cannot be measured shows `n/a` and raises a `probe failed` / `probe recovered` event (values and
badges from a failing probe are hidden, never left stale); the program never crashes on probe output.

## Safety

The binary only ever runs `ssh -G <host>` and `ssh -O check <host>`; every tool is looked up on `PATH`.
`scripts/live.sh` runs it with `guard/ssh` first on `PATH`, a shim that refuses any other ssh form and logs
the attempt to `guard/guard.log`.

## Development

```sh
scripts/check.sh          # fmt + clippy -D warnings + unit/L1 tests
scripts/check.sh --all    # + L2 tests (real binary against fake tools)
scripts/live.sh --once    # the binary against the live master, through the guard
```

- `crates/core` — pure parsers, detectors and the `Monitor` (no I/O; enforced by `clippy.toml`).
- `crates/app` — probes, main loop, CSV/JSON, ratatui TUI. L1 tests drive the loop on a paused clock;
  L2 tests run the binary with `fake-cmd` symlinked as every tool.
- `crates/testkit` — `Scenario` (what each command answers at time *t*) and `fake-cmd`.
- A `--record` tape replays in both layers by time.
