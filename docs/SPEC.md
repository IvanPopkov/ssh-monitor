# ssh-monitor — functional spec v2

## Scope
- Project `ssh-monitor` (this repo), binary `ssh-monitor`.
- Watches one host's ControlMaster from a macOS client.
- Read-only. **Never** opens an SSH connection.
- Host config (hostname, ControlPath, ServerAliveInterval) resolved via `ssh -G <host>`. No ControlPath → exit with error.
- **Master** = whichever process owns the control socket (as reported by `ssh -O check`). Usually the `ssh-master` supervisor's ssh, but a pane can win the race after an outage (`ControlMaster auto`).
- Optional source: `ssh-master` supervisor log `~/.local/state/ssh-master-<host>.log`. Missing → those features show `n/a`.
- Tailscale peer matched by IP from `ssh -G`, never by name (peer is named `deb`, not `fedora`).

## State machine
| State | Condition |
|---|---|
| DOWN | no master |
| DEGRADED | master alive AND any of: ≥3 consecutive lost pings · 5s median RTT > max(3× baseline, baseline + 100ms) · rx silent > 2× ServerAliveInterval + 5s (stall) |
| UP | otherwise; DEGRADED → UP only after 5s of clean pings |

Baseline = 5-min median RTT of samples taken while UP. Slow rule and spike events disabled until 10 samples. Baseline resets on path change.

## F1 · Status bar (1 row)
`●` light + state text · badges · host · master uptime (or `down X (was up Y)`) · time in current state · `PAUSED` · clock. No PID.
Uptime comes from `ps -o etime`; while that probe fails for the same PID it keeps counting from the last reading.

| Badge | Shown while |
|---|---|
| `⚠ STORM` (red) | any storm rule matches |
| `⚡ BREAKER` (red) | supervisor circuit breaker open |
| `↻ RETRY` (yellow) | reconnect loop active (see F5) |

## F2 · Throughput
- Two stacked charts of Braille columns (bottom → value): rx (received from the host), then tx (sent to it). 5-min history.
- Below 30 rows: one mirrored panel instead: rx columns grow up and tx columns grow down from a shared zero line. Title arrows show the data direction: `↓ rx` (download), `↑ tx` (upload).
- Fixed log Y axis, 100 B/s → 100 MB/s.
- Title per chart: current value + 5-min peak. In the merged panel, when that is too wide: `↓ rx 1.2 kB/s pk 3.4 kB/s  ↑ tx …`, then current values only.
- Gaps while DOWN.

## F3 · Latency
- now · min/avg/max · jitter (mean |ΔRTT|) · loss % — all over 5 min.
- Sparkline; lost pings shown as gaps. Every reply is at least `▁`, so a fast reply is never mistaken for a lost one.
- Path in panel title: `direct · LAN` / `direct · WAN` / `DERP <region>` / `relay <via>` / `n/a`.
  - direct ⇔ Tailscale `CurAddr` non-empty; LAN ⇔ `CurAddr` in a private range.
  - otherwise DERP via `Relay` region (`Relay` alone is only the home region — not proof of relaying).
  - Tailscale peer relay (`PeerRelay` set, no `CurAddr`) → `relay <via>`; peer offline or probe failing → `n/a`.
- Keeps pinging while DOWN.

## F4 · Clients
- Count + list, sorted, direct (`⚠`) clients first so an overflow never hides them. Source: `ssh` processes (argv0 = `ssh`) targeting the host, excluding the master.
- Labels: `dev-session N` → `#N` · VS Code Remote-SSH → `vscode` · other → argv after host, truncated (`scp`, `rsync`, `ssh`).
- Age per client. Direct (non-mux) client marked red `⚠`.
- List flows into columns; `+k more` on overflow.
- While DOWN: `no master` + any rogue ssh processes.
- No per-pane throughput (all channels share one socket — not measurable).

## F5 · Events
| Event | Severity |
|---|---|
| master up / restarted | green / yellow |
| master down — merged with matching supervisor line (±5s) when present: `master down after 5736s (rc=255)` | red |
| degraded (reason) / recovered (duration) | yellow / green |
| rtt spike — single ping above slow bar; muted while DEGRADED; ≤1 per 30s with ×N count | yellow |
| storm — (a) autossh present · (b) non-mux ssh: any process other than master with its own TCP to `<ip>:22`; once per culprit (PID shown) + `storm cleared after X` | red / cleared green |
| supervisor — `reconnecting in Ns` (dim) · `CIRCUIT OPEN` (red) · `CIRCUIT HALF-OPEN` (yellow) | — |
| reconnect loop — ≥3 consecutive master attempts that never reached UP, regardless of the log's own healthy/failure label: `reconnect loop: 3 attempts never connected (~75s each)`; `loop ended after N attempts` on first UP | yellow / green |
| path changed — direct ↔ not direct (DERP, peer relay, offline) or DERP region change only | dim |
| clients `+#N` / `−#N…` — one line per poll | dim |
| probe failed / recovered | yellow |
| system resumed after X — wall clock jumped ≥10s past monotonic (laptop sleep); gap = missing data | dim |

Display: newest first, colored by severity, no scrolling, 200 kept in memory.

## F6 · Keys
- `q` quit (also `Esc` and `Ctrl-C`).
- `p` freeze display only; sampling, events and CSV continue.
- `c` clear on-screen log; CSV unaffected.

## F7 · CLI
- `--host <alias>` — default `fedora`.
- `--interval <secs>` — base tick, default 2s (`--once`: 1s, so its 3s window fits three pings), min 0.5s, max 3600s. 1×: traffic, ping, CSV. 2×: status, clients, processes, supervisor log. 10×: tailscale. Render fixed at 250ms. Thresholds are wall-clock.
- `--log <path>` — append CSV, one row per tick:
  `ts,state,pid,rx_bps,tx_bps,rtt_ms,loss,path,clients,ssh_procs,events`
  (`events` = events fired that tick, joined by ` | `).
- `--once` — ~3s sample, print JSON snapshot, exit 0/1/2/3 = UP/DEGRADED/DOWN/error.
- `--record <dir>` — save every raw probe command + output as a JSONL tape (replayable in tests).

## F8 · Layout
- Borderless, rule-separated sections. Target ~16 rows × ≤90 cols; minimum 64×16.
- 30 rows and up: status 1 · rx title+2 · tx title+2 · latency|clients title+3 · events title+3 = 15; extra height split 2:2:1 to rx:tx:events.
- Below 30 rows (F2 merged panel): the rx and tx chart rows plus the tx title row form one panel, title + two equal halves (rx up, tx down); an odd row goes to events.
  At 16 rows: status 1 · rx/tx title+2+2 · latency|clients title+3 · events title+5.
- Below minimum: "terminal too small" message.

## F9 · Resilience
- Failing/missing probe → `n/a` + event. Never crash. Values and badges that depend on a failing probe are hidden, not left stale.

## Non-goals
Multiple hosts · remote-side metrics · actions on the connection (kill/restart) · Linux client · fixing the `ssh-master` supervisor (separate task).
