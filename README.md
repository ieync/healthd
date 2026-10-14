# healthd

A small system-health daemon for **Artix Linux (dinit)**. It watches your NVMe
drive, battery, CPU and RAM, keeps a local SQLite history, and pops up a desktop
notification when something degrades or is being abused.

* Stable Rust, no async runtime — plain `std::thread` + sleep.
* Reads **sysfs / procfs** wherever possible (no root). The one exception is NVMe
  SMART data, which needs a single narrow `doas` rule.
* Idle cost: about **5 MB RSS** and **~0 % CPU** (measured, see
  [Resource budget](#resource-budget)).
* Frontends are pluggable: `healthctl` (CLI) and `health-tui` (ratatui) both talk
  to the daemon **only** through its Unix-socket JSONL API — never to the database.

## What it watches

| Source    | Metrics                                                                                       | How / how often                                                              | Stored in SQLite                    |
|-----------|-----------------------------------------------------------------------------------------------|------------------------------------------------------------------------------|-------------------------------------|
| `cpu`     | `temp_c`, `load_pct`, `throttle_events`                                                        | `/proc/stat` + `/sys/class/hwmon` every 5 s, held in RAM                      | 1-minute average (throttle: summed) |
| `ram`     | `available_mb`, `oom_headroom_mb`, `swap_used_pct`, `psi_some_avg10`                           | `/proc/meminfo` (+ PSI) every 5 s, held in RAM                                | **no samples**; alerts → `events`   |
| `battery` | `charge_pct`, `health_pct` (full ÷ design), `cycles`, `hours_held_full`, `temp_c`, `discharging_pct` | `/sys/class/power_supply/BAT0/uevent` every 60 s                              | every minute                        |
| `nvmeN`   | `temp_c` · `percentage_used`, `avail_spare`, `media_errors`, `critical_warning`, `unsafe_shutdowns` | temperature from `/sys/block/nvme*/device/hwmon*` every 60 s; SMART via `doas -n nvme smart-log` every 10 min | every poll                          |

Notes:

* `battery.discharging_pct` is only emitted while discharging (so a "deep
  discharge" rule cannot fire on AC) and is live-only.
* `battery.hours_held_full` counts hours spent at ≥ 95 % while not discharging —
  the classic way to wear a laptop battery. It restarts when the daemon restarts.
* `cpu.throttle_events` uses Intel's `thermal_throttle` counters. AMD does not
  expose them, so on AMD that metric is simply absent.
* Metrics a machine does not have (no battery, no PSI, no swap) are skipped, not
  errors.

## How it fits together

```
  cpu ─────┐
  ram ─────┤  pollers: one supervised
  battery ─┤  thread each
  nvme ────┘
     │ readings
     ▼
  ┌──────────────────────┐  alert   ┌──────────────────────┐
  │ Hub                  │ ───────▶ │ notify-rust          │──▶ desktop
  │  live values (RAM)   │          │ (own thread, D-Bus)  │    notification
  │  rule engine         │          └──────────────────────┘
  └───┬──────────────▲───┘
      │ samples,     │ status / history / events
      │ events       │
      ▼              │
  ┌──────────────┐  ┌┴─────────────┐   JSONL over           ┌───────────┐
  │ SQLite (WAL) │  │ ipc server   │◀── ~/.run/healthd.sock ─│ healthctl │
  │ 1 connection │  │ (read-only)  │                        │ health-tui│
  └──────────────┘  └──────────────┘                        └───────────┘
```

| Path                         | Role                                                                                |
|------------------------------|-------------------------------------------------------------------------------------|
| `src/main.rs`                | Wiring: args, config, DB, spawn pollers, serve IPC                                  |
| `src/config.rs`              | TOML config (retention, battery name, nvme command, notify switch, alert rules)     |
| `src/db.rs`                  | The one SQLite connection: inserts, queries, retention, hourly downsampling         |
| `src/ipc.rs`                 | Unix-socket JSONL server (read-only API)                                            |
| `src/alert.rs`               | Rule engine (sustain, cooldown, recovery) and the notify-rust notifier thread       |
| `src/poll/{cpu,ram,battery,nvme}.rs` | One poller each, with pure parse functions that are unit-tested           |
| `src/poll/mod.rs`            | Shared `Hub`, supervisor (restarts a crashed poller), drift-free ticker             |
| `src/bin/healthctl.rs`       | CLI frontend                                                                        |
| `src/bin/health-tui.rs`      | ratatui frontend (the only code that links ratatui)                                 |
| `crates/healthd-proto`       | Wire types + blocking client + formatting helpers shared by all frontends           |
| `config/healthd.toml`        | The default config, embedded in the binary and installed as an example              |
| `dinit/healthd.in`           | dinit service template                                                              |

## Install

Requirements: stable Rust, a C compiler (SQLite is bundled and compiled in), and a
running notification daemon (dunst, mako, your desktop's own, …) for pop-ups.

```sh
make build
make install            # to ~/.local/bin and ~/.config/…, no root needed
```

`make install` puts three binaries in `~/.local/bin` (make sure that is in your
`PATH`), the dinit service in `~/.config/dinit.d/healthd`, and a commented example
config in `~/.config/healthd/config.toml.example`. It never overwrites your own
`config.toml`. Use `PREFIX=…` or `DESTDIR=…` to install elsewhere.

### Start it with dinit

healthd belongs in your **user** dinit instance (it needs your desktop's D-Bus
session bus and writes to `$HOME`):

```sh
dinitctl enable healthd     # start with your session
dinitctl start healthd
dinitctl catlog healthd     # its log output
```

`enable` assumes your user instance has the usual `boot` service with a
`waits-for.d` directory; otherwise add `waits-for = healthd` to your `boot`
service. If notifications never show up, the service can't find your session bus —
see the comment at the top of `dinit/healthd.in` (`dinitctl setenv
DBUS_SESSION_BUS_ADDRESS=…`).

You can also just run `healthd` in a terminal to try it.

### NVMe SMART access (the one doas rule)

Wear and error counters (`percentage_used`, `avail_spare`, `media_errors`,
`critical_warning`, `unsafe_shutdowns`) can only be read from the drive as root.
healthd runs, as you, every 10 minutes and per controller:

```
doas -n nvme smart-log /dev/nvme0 -o json
```

`-n` means *never prompt*: without a matching rule the call just fails. Allow
exactly that command in `/etc/doas.conf`:

```
permit nopass youruser as root cmd nvme args smart-log /dev/nvme0 -o json
```

(one line per controller; `ls /sys/block | grep nvme` shows yours). doas matches
the command **as typed**, so the `cmd` in the rule must equal the `[nvme] command`
in healthd's config (default `nvme`). If you prefer an absolute path in the rule,
use `cmd /usr/bin/nvme` **and** set `command = "/usr/bin/nvme"`. Check it works
before starting the daemon: run the command above by hand — it must print JSON and
must not ask for a password.

Without the rule healthd still works: temperature comes from sysfs, and one
`warn` event ("SMART data unavailable …", including the exact command it tried) is
logged per controller instead of repeating every 10 minutes.

## Use it

```sh
healthctl status                                   # latest values + active alerts
healthctl history cpu temp_c --since 6h            # summary + sparkline
healthctl history battery health_pct --since 30d --raw   # timestamp<TAB>value lines
healthctl events --since 7d --min-severity warn
healthctl status --json | jq .data                 # any command takes --json
health-tui                                          # live dashboard
```

`--since` takes `90s`, `30m`, `6h`, `7d`, `2w`, or a Unix timestamp. Times are
shown in UTC (ISO-8601 with a `Z`). `healthctl` exits non-zero with a clear message
if the daemon is not running.

`health-tui` keys: `↑/↓` or `j/k` choose a metric, `w` cycles the chart window
(1 h / 6 h / 24 h / 7 d), `r` refreshes, `q` / `Esc` / `Ctrl-C` quits. It
reconnects by itself if the daemon restarts.

Both accept `--socket PATH`; the default is `$HEALTHD_SOCK` or `~/.run/healthd.sock`.

## Configuration

With no config file the built-in defaults run. To customise, copy the example and
edit it — `healthd` reads `$XDG_CONFIG_HOME/healthd/config.toml` (default
`~/.config/healthd/config.toml`) at startup:

```sh
cp ~/.config/healthd/config.toml.example ~/.config/healthd/config.toml
dinitctl restart healthd
```

(`healthd --print-default-config` prints the same file.) A typo anywhere — an
unknown key, a bad operator — stops the daemon with the line number rather than
silently ignoring a rule.

> If your file contains **any** `[[rule]]`, your rules **replace** the built-in
> set entirely. Copy the rules you still want.

The default rules (each threshold and interval is explained in
`config/healthd.toml`). The first five are the ones you asked for:

| Rule                       | Fires when                                  | Severity | Sustained for        | Repeat      |
|----------------------------|---------------------------------------------|----------|----------------------|-------------|
| `cpu-hot`                  | `cpu temp_c > 85`                           | warn     | 15 s (3 × 5 s)       | 30 min      |
| `nvme-spare-low`           | `nvme* avail_spare < 20` (%)                | crit     | first sample         | 24 h        |
| `nvme-media-errors`        | `nvme* media_errors > 0`                    | crit     | first sample         | 24 h        |
| `nvme-hot`                 | `nvme* temp_c > 75`                         | warn     | 2 min (2 × 60 s)     | 30 min      |
| `battery-worn`             | `battery health_pct < 80` (full ÷ design)   | warn     | first sample         | 7 d         |
| `ram-low`                  | `ram available_mb < 500`                    | warn     | 10 s (2 × 5 s)       | 15 min      |
| `cpu-pegged` *(extra)*     | `cpu load_pct > 95`                         | warn     | 5 min                | 60 min      |
| `cpu-throttled` *(extra)*  | `cpu throttle_events > 0`                   | warn     | first sample         | 60 min      |
| `nvme-critical-warning` *(extra)* | `nvme* critical_warning > 0`         | crit     | first sample         | 24 h        |
| `nvme-worn` *(extra)*      | `nvme* percentage_used >= 90`               | warn     | first sample         | 7 d         |
| `battery-hot` *(extra)*    | `battery temp_c > 45`                       | warn     | 3 min                | 60 min      |
| `battery-held-full` *(extra)* | `battery hours_held_full > 24`           | info     | first sample         | 7 d         |
| `battery-deep-discharge` *(extra)* | `battery discharging_pct < 10`      | warn     | first sample         | 30 min      |
| `ram-oom-risk` *(extra)*   | `ram oom_headroom_mb < 300` (RAM + free swap) | crit   | 10 s                 | 15 min      |
| `ram-thrashing` *(extra)*  | `ram psi_some_avg10 > 25`                   | warn     | 30 s                 | 30 min      |

A rule that keeps being violated notifies again only after its repeat interval; when
the value returns to normal an `info` "back to normal" event is recorded (no pop-up).
`nvme*` is a prefix match, so each drive is tracked separately.

Notifications use urgency by severity: `info` low, `warn` normal, `crit` critical
(stays on screen until dismissed). Set `[notify] desktop = false` for a headless
box: alerts are still written to the events table.

## Data

* `~/.local/share/healthd/health.db` (`$XDG_DATA_HOME` honoured), WAL mode,
  directory `0700`.
* `samples(ts, source, metric, value)` and `events(ts, severity, source, msg)`.
* **30-day retention.** Samples older than 48 h are averaged into hourly rows, so
  a month of 1-minute data stays at a few MB. Both windows are configurable under
  `[retention]`. Maintenance runs at startup and every 6 hours.
* One SQLite connection, cached prepared statements, a ~1 MiB page cache and
  `synchronous = NORMAL` (fewer fsyncs = less wear on the SSD this daemon is
  watching). Disk writes are a handful of rows per minute.
* Nothing outside the daemon opens the database; to look at data use `healthctl`.
  (The `sqlite3` CLI works read-only too, if you really want it.)

## Socket API

Newline-delimited JSON on `~/.run/healthd.sock` (directory `0700`, socket `0600`).
One request per line, one reply per line; a connection can be reused. Read-only.

```
-> {"cmd":"status"}
<- {"ok":true,"data":{"active_alerts":[],"metrics":[{"metric":"available_mb","source":"ram","ts":1791888628,"value":7408.29,"...":"..."}],"uptime_secs":1,"version":"0.1.0"}}

-> {"cmd":"history","source":"cpu","metric":"load_pct","since":1791880000,"limit":500}
<- {"ok":true,"data":{"metric":"load_pct","points":[{"ts":1791888475,"value":2.0}],"source":"cpu"}}

-> {"cmd":"events","since":1791880000,"limit":50,"min_severity":"warn"}
<- {"ok":true,"data":{"events":[{"ts":1791888395,"severity":"crit","source":"ram","msg":"…"}]}}

-> {"cmd":"nope"}
<- {"ok":false,"error":"bad request: unknown variant `nope`, expected one of `status`, `history`, `events` …"}
```

`since` is a Unix timestamp (default: 24 h ago); `limit` keeps the *newest* points
(`history` ≤ 10 000, `events` ≤ 1 000). Requests longer than 4 KiB, more than 8
simultaneous clients, or 60 s of silence are cut off. From a shell:
`printf '{"cmd":"status"}\n' | socat - UNIX-CONNECT:$HOME/.run/healthd.sock`.

To write your own frontend (a status-bar module, say), use this protocol directly,
or depend on the `healthd-proto` crate for typed requests and a blocking client.

## Resource budget

Measured on the release build (`opt-level = "s"`, LTO, stripped; `healthd` is
3.0 MB on disk), idle, in a container where only `/proc` was available:

| State                               | VmRSS    | Anonymous | Threads | CPU (30 s window) |
|-------------------------------------|----------|-----------|---------|-------------------|
| Idle, nothing to report             | 5.0 MB   | 0.5 MB    | 7       | ≤ 0.03 % (1 clock tick) |
| After the first notification (D-Bus connection open) | 5.4 MB | 0.6 MB | 9 | –            |

How: no async runtime; every poller sleeps in `thread::sleep` on a drift-free
ticker; the IPC server blocks in `accept`; the notifier thread blocks on a channel
and only opens D-Bus when the first alert fires; poller and connection thread stacks are 256 KiB (1 MiB for the notifier, whose D-Bus library builds messages on its stack); the
live values never touch SQLite. CPU/battery/NVMe sensors were not present in that
container, but their cost is a few tiny sysfs reads per interval. Check on your
own machine:

```sh
grep -E 'VmRSS|Threads' /proc/$(pgrep -x healthd)/status
```

## Development

```sh
make fmt      # rustfmt
make lint     # rustfmt --check + clippy -D warnings
make test     # unit tests (parsers, rule engine, DB retention, IPC round trip, TUI rendering)
```

Conventions: every module and function has a doc comment, and every threshold and
polling interval carries a `WHY` comment saying why that number was chosen.

Design notes and limitations:

* **No signal handler.** SIGTERM just ends the process; SQLite's WAL makes that
  crash-safe and a stale socket is replaced on the next start. (Handling signals
  would need a `libc`/`signal-hook` dependency for no real gain.)
* **Supervised pollers.** If a poller panics, the supervisor records a `crit`
  event and restarts it after 30 s, so one bad parser can't silently end monitoring.
* **Single battery.** Only the battery named in `[battery] name` (default `BAT0`)
  is watched.
* **Times are UTC** in the CLI/TUI; the database stores Unix seconds.
* The dinit service file passes `dinit-check` (dinit 0.23.0pre, built from
  source) with no problems, but it has not been run under a live Artix dinit
  instance. If your dinit version rejects a property, the file is short and every
  line is commented. Run `dinit-check -d ~/.config/dinit.d healthd` after
  installing to check it against your own dinit.
* Notifications were tested end to end against a mock `org.freedesktop.Notifications`
  server on a private session bus (critical urgency, no timeout, one pop-up per
  cooldown period), and with no server at all (one logged error; the event is
  still recorded and the daemon keeps running).
