# ssh-monitor — architecture v1

Companion to [SPEC.md](SPEC.md). Principle: **functional core, imperative shell** — all logic is pure and fed explicit timestamps; I/O lives in a thin shell.

## Crate map
```
ssh-monitor/
├─ Cargo.toml              # [workspace]
├─ crates/
│  ├─ core/                # PURE. deps: serde, serde_json only
│  │  ├─ clippy.toml       # bans clock / process / fs / net / thread / print
│  │  └─ src/
│  │     ├─ parse/         # &str → typed sample (ssh -G, ssh -O check, nettop, ps etime, ping, ps -axo, lsof, supervisor log, tailscale)
│  │     ├─ monitor/       # Monitor + detectors (below); known.rs = what is current (F9) + badges
│  │     ├─ ring.rs        # fixed-capacity ring buffer
│  │     └─ tape.rs        # JSONL tape types + read/write
│  ├─ app/                 # bin `ssh-monitor`. deps: tokio, ratatui, anyhow, serde_json, time
│  │  └─ src/
│  │     ├─ cli.rs         # args
│  │     ├─ config.rs      # host resolution via `ssh -G`
│  │     ├─ clock.rs       # mono + wall clock, UTC offset
│  │     ├─ runner.rs      # CommandRunner trait + tokio::process impl + recording wrapper
│  │     ├─ probes.rs      # one task per probe, own interval
│  │     ├─ app.rs         # main loop (owner of Monitor), keys, frontends
│  │     ├─ ui/            # ratatui layout + formatting (units, durations, log axis)
│  │     ├─ snapshot.rs    # --once JSON + exit codes
│  │     ├─ csv.rs         # --log
│  │     └─ main.rs        # wiring
│  └─ testkit/             # dev-only, publish = false
│     └─ src/              # Scenario builder, bin `fake-cmd`, time-based tape replay (FakeRunner/TapeRunner adapters: crates/app/tests/common)
├─ guard/ssh               # shim: only `-O check` / `-G` / `-V` reach real ssh; refusals → guard/guard.log
└─ scripts/                # check.sh (all gates), live.sh (run binary with guard), pre-commit
```

Package names: `ssh-monitor-core` (folder `crates/core` — a crate can't be named `core`), `ssh-monitor` (bin),
`ssh-monitor-testkit`. L1 and L2 tests live in `crates/app/tests/`; L2 test functions are named `l2_*`.

Dependency direction: `app → core`, `testkit → core`, `app (dev) → testkit`. Core depends on nothing internal.

## Data flow
```
probe tasks ──(Stamp, ProbeResult)──┐     each: CommandRunner.run(cmd, timeout) → parse → send
input thread ──Key──────────────────┤     std thread, blocking crossterm read
render tick (250ms) ────────────────┤
                                    ▼
             main loop (tokio::select!) — sole owner of Monitor
               ├─ monitor.apply(result, stamp) → Vec<Event>
               ├─ draw(&monitor)            (skipped while paused)
               └─ csv row per base tick
```
- One-shot commands only (`ping -c 1 -t 1`, `nettop -L 1 …`, `ssh -O check`, …). Timeout = failure / lost ping.
- All commands resolved via `PATH` — never absolute paths (required for L2).
- Host config resolved once at startup via `ssh -G <host>`.

## Probes (verified on macOS 27.0.1, OpenSSH 10.3p1, Tailscale 1.104.1)
| Probe | Command | Cadence | Cost | Parsing notes |
|---|---|---|---|---|
| config | `ssh -G <host>` | startup | — | ignore stderr (spurious "Pseudo-terminal…" line) |
| status | `ssh -O check <host>` | 2× | 0.01s | message on **stderr**, ends `\r\n`: `Master running (pid=N)`; down → exit 255 `Control socket connect(…): No such file or directory` |
| traffic | `nettop -p <pid> -P -L 1 -x -J bytes_in,bytes_out` | 1× | 0.14s (sometimes ~5s; timeout 8s) | CSV `,bytes_in,bytes_out,` + `ssh.<pid>,<in>,<out>,` (trailing comma). Dead PID → exit 0, header only ⇒ no data, not zero |
| uptime | `ps -o etime= -p <pid>` | 2× | — | `mm:ss`, `hh:mm:ss`, `dd-hh:mm:ss` (last not captured live — fixture hand-written). Bad PID → exit 1 |
| latency | `ping -c 1 -t 1 <ip>` | 1× | 0.05s | exit 0 + `time=X ms`; lost → exit 2 after ~1s |
| clients | `ps -axo pid=,ppid=,etime=,command=` | 2× | — | filter argv0 = `ssh` (excludes supervisor bash, VS Code helper whose JSON args contain the host) |
| rogues | `lsof -n -P -iTCP@<ip>:22` | 2× | 0.08s | any PID ≠ master = own TCP connection. Panes that became master share the master's socket (same DEVICE) |
| supervisor | `tail -n 50 <log>` | 2× | — | line shapes: `opening master connection` · `master ended after Ns (rc=N) — …` · `master died after Ns (rc=N) — failure N/N` · `reconnecting in Ns` · `CIRCUIT OPEN…` / `HALF-OPEN`. Ignore its healthy/failure labels |
| path | `tailscale status --json` | 10× | 0.06s | peer by IP; direct ⇔ `CurAddr` ≠ ""; `Relay` = home DERP region |

Rejected: `lsof -p <pid> -U` — ORs selectors (lists every unix socket on the system); with `-a` still ambiguous (fd-passed sockets of mux clients). `pgrep -f <host>` — matches non-ssh processes.

## Time
Every input carries `Stamp { mono: Duration, wall: unix_ms }`.
- `mono` → all rules (hysteresis, stall, cooldowns, 5-min windows).
- `wall` → event timestamps, CSV.
- `wall − mono` jump ≥ 10s → `system resumed` event + data gap. (macOS `Instant` stops during sleep.)

## Monitor & detectors
`Monitor::apply(input, stamp) -> Vec<Event>` routes to detectors; each owns its state, reads others via read-only snapshot, never mutates them.

| Detector | Owns |
|---|---|
| `health` | UP/DEGRADED/DOWN, hysteresis, RTT baseline |
| `traffic` | counter diffs → rates, rx/tx rings, stall |
| `latency` | RTT ring, stats, spike cooldown |
| `storm` | autossh / non-mux ssh (own TCP); onset + clear |
| `supervisor` | log lines → events, breaker state, reconnect-loop detection, merge into `master down` |
| `clients` | client set diff → coalesced events, labels |
| `path` | direct ↔ DERP |
| `probe_health` | per-probe failed / recovered |
| `sleep` | resume detection |

Core exposes raw state plus what is currently known (`known.rs`: `known_*`, `badges`); `app/ui` does all formatting.

## Errors
- Core: typed enums, no error crates. Parse failure → `ProbeResult::Failed(reason)` → `probe failed` event.
- App: `anyhow`. Never panics on probe output.

## Test layers
| Layer | What | Where | Runs in |
|---|---|---|---|
| Unit | parsers (real captured fixtures), detectors, ring, rates | `core` | hook + CI |
| L1 | whole app loop: FakeRunner tape, tokio paused clock, ratatui `TestBackend` asserts, CSV | `app` tests | hook + CI |
| L2 | real binary, `fake-cmd` symlinked as ssh/nettop/ping/… on PATH; asserts `--once` JSON + exit code, `--log` CSV; short real-time scenarios (~10s) | `tests/` | CI |
| Real capture | `--record` during a one-time real incident (Tailscale off ~60s) → tape → replayed by L2 — **not done yet** (see Open items) | manual, once | — |

Tape (JSONL, shared by `--record`, FakeRunner, fake-cmd, Scenario builder):
```
{"at_ms":0,"cmd":["ssh","-O","check","fedora"],"exit":0,"stdout":"","stderr":"Master running (pid=48001)\r\n","delay_ms":4}
```

## Quality gates
- `scripts/check.sh` = fmt check + clippy `-D warnings` + unit/L1 tests; `--all` adds L2. Single source for:
- **Pre-commit hook** (`scripts/install-hooks.sh`): `scripts/check.sh`.
- **GitHub Actions** (macos-latest): `scripts/check.sh --all`.

## Live-run guard
`scripts/live.sh` puts `guard/ssh` first on `PATH`: only `ssh -O check` / `-G` / `-V` reach the real ssh, anything
else is refused and logged to `guard/guard.log`. Self-test: `guard/test.sh`.

## Decision log
| # | Decision | Why |
|---|---|---|
| A0 | Unit + L1 + L2 test layers | Logic, wiring and process boundary each covered |
| A0b | `--record` + one real capture instead of container L3 | Real stack (incl. Tailscale/DERP) for less effort; L3 only adds ~3 ssh-behavior facts |
| A1 | Strict core/shell; time injected | Deterministic, runtime-free tests of every spec rule |
| A2 | Workspace split + core `clippy.toml` bans | Compiler blocks async/TUI deps in core; clippy blocks std I/O. `clippy.toml` is per-crate, so a split is required |
| A3 | Core exposes raw state; app formats | User choice. Formatters stay pure + unit-tested in `app`, but outside clippy enforcement |
| A4 | All probes one-shot, single `CommandRunner` | One interface for real / fake / record. Risk: slow `nettop -L 1` → revisit nettop only |
| A5 | Probe tasks → mpsc → single owner loop | No locks; slow probe delays only itself |
| A6 | `Stamp { mono, wall }` | Rules immune to clock jumps; timestamps correct after laptop sleep |
| A6b | `system resumed` event | Explains master restarts after sleep |
| A7 | One JSONL tape format | A real recording replays in L1 and L2 unchanged |
| A8 | Tape in core; `testkit` dev-only crate | Test helpers never ship; no awkward app → testkit dependency |
| A9 | One detector per concern | Each spec rule maps to one file + its tests |
| A10 | Pre-commit + GitHub Actions (macos-latest) | Fast local feedback; CI runs on the platform the tool targets |
| A12 | This doc | Visibility on what and why |
| P1 | Read `ssh-master` supervisor log; `⚡ BREAKER` badge; drop storm rule "master flapping" | Supervisor already decides on flapping; its log has drop reasons + rc |
| P2 | Clients from `ps`; rogues from `lsof -iTCP@<ip>:22` | `lsof -U` ambiguous; post-outage capture showed `-iTCP` flags exactly the 2 extra connections |
| S1 | Client labels `#N` / `vscode` / argv; `⚠` on direct clients | VS Code went direct after the outage — must be visible |
| S2 | Merge supervisor line into `master down` | Keep the 3-line event panel readable during drops |
| S3 | `path changed` only on direct ↔ DERP; LAN/WAN in panel title | `CurAddr` changes with NAT/network without a path-kind change |
| S4 | Reconnect-loop detection + `↻ RETRY` badge | Supervisor logged 75 connect timeouts (75s ≥ its 60s threshold) as "healthy"; its breaker never tripped |

## Open items
- Real `--record` capture of an incident (test layer "Real capture").
