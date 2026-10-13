# Probe fixtures

Real outputs captured on the developer's Mac (macOS 27.0.1, OpenSSH 10.3p1, Tailscale 1.104.1),
2026-10-09 → 2026-10-12, unless marked **hand-written**. Byte-exact (note `\r\n` in ssh stderr).

Sanitized: Tailscale IPs → `100.100.1.10` (host `fedora`) / `100.100.1.20` (this Mac);
public IP → `203.0.113.46`; Tailscale JSON trimmed to the fields we parse, keys/emails removed.

| File | Command | Exit | Notes |
|---|---|---|---|
| `ssh_G.txt` | `ssh -G fedora` (stdout) | 0 | stderr had a spurious "Pseudo-terminal will not be allocated" line |
| `ssh_check_up.stderr` | `ssh -O check fedora` (**stderr**) | 0 | stdout empty; master run by `ssh-master` supervisor |
| `ssh_check_up_mux.stderr` | same, after outage | 0 | master was a pane's `ControlMaster auto` process (`ssh: … [mux]`) |
| `ssh_check_down.stderr` | `ssh -o ControlPath=/var/empty/nope.sock -O check fedora` | 255 | no master |
| `nettop_ok.csv` | `nettop -p <pid> -P -L 1 -x -J bytes_in,bytes_out` | 0 | row `ssh.<pid>,<in>,<out>,` |
| `nettop_dead_pid.csv` | same, PID that does not exist | **0** | header only ⇒ "no data", not zero |
| `etime_mmss.txt` / `etime_hhmmss.txt` / `etime_ddhhmmss.txt` | `ps -o etime= -p <pid>` | 0 | three formats; bad PID → exit 1, `ps: process id too large: …` |
| `ping_ok.txt` | `ping -c 1 -t 1 <ip>` | 0 | |
| `ping_lost.txt` | `ping -c 1 -t 1 192.0.2.1` | 2 | took 1.0s |
| `ps_axo_normal.txt` | `ps -axo pid=,ppid=,etime=,command=` | 0 | **grep-filtered** to rows containing `ssh `; 2 panes + VS Code (mux) + master |
| `ps_axo_postoutage.txt` | same, after outage | 0 | trimmed: 12 unrelated rows + all ssh-related rows. Pane master + supervisor duplicate + VS Code direct |
| `lsof_tcp_normal.txt` | `lsof -n -P -iTCP@<ip>:22` | 0 | master only (transcribed from terminal) |
| `lsof_tcp_postoutage.txt` | same, after outage | 0 | 4 rows / 3 sockets: pane+mux master share a socket, supervisor dup, VS Code direct |
| `supervisor_outage.log` | `~/.local/state/ssh-master-fedora.log` | — | 399 lines: 75s connect-timeout loop logged as "healthy", then 45s failures |
| `supervisor_breaker.log` | — | — | **hand-written** from `ssh-master` source: breaker open / half-open (never observed) |
| `tailscale_status_direct_wan.json` | `tailscale status --json` (trimmed) | 0 | direct, public `CurAddr` |
| `tailscale_status_direct_lan.json` | same, after outage | 0 | direct, LAN `CurAddr` |
| `tailscale_status_derp.json` | — | — | **hand-written**: WAN capture with `CurAddr=""` (DERP-relayed via `par`; never observed) |
