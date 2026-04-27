# Using stax from an agent

stax is built around a long-running unprivileged daemon, **stax-server**, that
hosts the run registry, the live aggregator, and three vox services. Both
human users on the CLI and AI agents talk to the same surface.

This document is the agent-facing manual: install, lifecycle, query, wait.

## Install

One command from a fresh checkout:

```
cargo xtask install
```

…builds release binaries for `stax`, `staxd`, and `stax-server`, copies them
to `~/.cargo/bin/`, ad-hoc codesigns `stax`, and **bootstraps `stax-server` as
a per-user LaunchAgent** so it's running on login.

After that, one privileged step (the only `sudo` you'll ever do):

```
sudo stax setup
```

…installs `staxd` (the privileged kperf/kdebug owner) as a LaunchDaemon. From
this point on, `stax record …` is unprivileged.

### What runs where

| component     | privilege  | launchd kind      | socket                                     |
|---------------|------------|-------------------|--------------------------------------------|
| `staxd`       | root       | LaunchDaemon      | `/var/run/staxd.sock`                      |
| `stax-server` | user       | LaunchAgent       | `$XDG_RUNTIME_DIR/stax-server.sock` or `/tmp/stax-server-$UID.sock` |
| `stax`        | user       | (CLI)             | (no socket)                                |

`stax-server` also binds **`ws://127.0.0.1:8080`** for the web UI. Override
with `STAX_SERVER_WS_BIND=host:port` (set in the LaunchAgent plist's
`EnvironmentVariables` if you want it persistent).

### Logs

- `staxd`        → `/var/log/staxd.log`
- `stax-server`  → `~/Library/Logs/stax-server.log`

### Verifying the install

```
stax status            # talks to stax-server, reports active run
launchctl list eu.bearcove.staxd        # should show pid + 0
launchctl list eu.bearcove.stax-server  # should show pid + 0
```

## Concurrency model

**One active run at a time.** `stax-server` rejects a second `start_run` while
one is in flight. If you hit this, your options are:

```
stax wait     # block until the active run stops
stax stop     # ask stax-server to stop it now
```

Historical runs stay queryable via `stax list` (in-memory only for now;
persistence is a follow-up).

## Lifecycle from an agent's POV

Typical agent flow:

```
stax record -- ./bench         # 1. start a recording (blocks until done,
                               #    or use `&` to background it)

stax wait --for-samples 10000  # 2. block until enough samples land
                               #    (or --for-seconds N, --until-symbol foo)

stax top -n 20 --sort self     # 3. inspect the hot leaf functions

stax annotate 0x10004ad60      # 4. get per-instruction sample counts +
                               #    interleaved source for one function
```

If you need to abort:

```
stax stop
```

## Subcommands reference

All subcommands connect to `stax-server` via its local socket. They fail
loudly if the daemon isn't running.

### `stax record [-- COMMAND…]`

Start a recording. Either launch a child:

```
stax record -- ./target/release/foo --bench bar
```

…or attach to an existing process:

```
stax record --pid 12345
```

Useful flags:

- `-F, --frequency <HZ>` — PET sampling rate (default 900)
- `-l, --time-limit <SECS>` — stop after N seconds (otherwise Ctrl-C)
- `--daemon-socket <PATH>` — override `staxd`'s socket
- `--serve <ADDR>` — *legacy* in-process WS aggregator. Skips
  stax-server entirely; only useful when you don't want the daemon.

When the daemon is running and `--serve` is **not** passed, stax-server
gets every PET sample, off-CPU interval, wakeup edge, binary load, and
thread name event over the local socket.

### `stax status`

Snapshot of the daemon. Prints the active run if any, plus when the
daemon itself started.

```
$ stax status
active run:
  run 1  [recording]  pid 12345  4824 samples / 119 intervals  (./bench)
```

### `stax list`

Every run the daemon has hosted (active + history, oldest first).

```
$ stax list
  run 1  [stopped]  pid 11000  9421 samples / 244 intervals  (./bench)
  run 2  [recording]  pid 12345  4824 samples / 119 intervals  (./bench)
```

### `stax wait [--for-samples N | --for-seconds N | --until-symbol NEEDLE] [--timeout-ms MS]`

Block until a condition fires, the active run reaches `Stopped`, or
the optional hard `--timeout-ms` elapses.

| flag                 | meaning                                                            |
|----------------------|--------------------------------------------------------------------|
| (none)               | wait for the active run to stop                                    |
| `--for-samples N`    | return after at least N PET samples have been ingested             |
| `--for-seconds N`    | return after N seconds of wall-clock time                          |
| `--until-symbol S`   | return once a symbol containing S has been seen (case-sensitive)   |
| `--timeout-ms MS`    | hard cap on the whole wait; exit code 1 + “timed out” message      |

Mutually exclusive across the first three (pass at most one).

```
$ stax wait --for-samples 5000 --timeout-ms 10000
condition met:
  run 2  [recording]  pid 12345  5012 samples / 124 intervals  (./bench)
```

Exit codes:

| code | situation                                            |
|------|------------------------------------------------------|
| 0    | condition met, or run reached `Stopped` cleanly      |
| 1    | timed out, or no active run, or other error          |

### `stax stop`

Ask the daemon to stop the active run cleanly. Prints the final
summary.

```
$ stax stop
stopped:
  run 2  [stopped]  pid 12345  5012 samples / 124 intervals  (./bench)
```

Exits non-zero if there's no active run.

### `stax top [-n N] [--sort self|total] [--tid TID]`

Snapshot the top-N hottest functions in the active run.

- `--sort self` (default) — leaf-only attribution (where the program is
  *now*).
- `--sort total` — any-frame attribution (functions that *contain* hot
  code, including their callers).

Output is one line per entry: `<self ms> <self samples> <function> (<binary>)`.

```
$ stax top -n 5
   42.184ms       3812 samples  vox_jit::translate (libvox.dylib)
    9.001ms        812 samples  cranelift::lower (libcranelift.dylib)
    …
```

### `stax annotate <ADDR> [--tid TID]`

Disassemble the function containing `ADDR` (hex with `0x` prefix or
decimal) and annotate every instruction with self-attribution counts.
Source lines are interleaved when DWARF is present and the file is
readable.

```
$ stax annotate 0x10004ad60
; vox_jit::translate (rust) @ 0x10004ad58
; src/translate.rs:412
  0x10004ad58      0 samples    push rbp
  0x10004ad59      0 samples    mov  rbp, rsp
  0x10004ad5c     14 samples    mov  rax, qword ptr [rsi]
  …
```

The `--tid` flag filters to one thread. Omit for whole-process.

### `stax setup`

Privileged install of `staxd` (LaunchDaemon). Run once with `sudo`.
Not part of the routine agent flow.

## Wire / RPC services

Programmatic clients can skip `stax` and talk to the daemon's vox services
directly. All three live in `stax-live-proto`:

- **`RunControl`** — agent lifecycle (`status`, `list_runs`, `wait_active`,
  `stop_active`).
- **`RunIngest`** — recorder-side ingest (`start_run` with a
  `Rx<IngestEvent>` channel). Agents shouldn't need this.
- **`Profiler`** — query surface (`top`, `subscribe_top`,
  `subscribe_flamegraph`, `subscribe_annotated`, `subscribe_neighbors`,
  `subscribe_threads`, `subscribe_timeline`, …). The `subscribe_*` variants
  push periodic updates over a `vox::Tx<…>`; receive a single update for a
  one-shot snapshot.

Connect with:

- `local://$XDG_RUNTIME_DIR/stax-server.sock` (or `/tmp/stax-server-$UID.sock`),
  for trusted local agents
- `ws://127.0.0.1:8080`, for browser clients (TS bindings live in
  `frontend/src/generated/`)

## Common pitfalls

- **`error: stax-server isn't running`** — the LaunchAgent isn't loaded.
  `cargo xtask install` does this; or by hand:

      launchctl bootstrap "gui/$(id -u)" \
        ~/Library/LaunchAgents/eu.bearcove.stax-server.plist

- **`another run is already active`** — single-active-run model. Use
  `stax wait` or `stax stop` first.

- **`stax record` says “stax-server unreachable”** — daemon's down.
  Recording proceeds but events go nowhere; no agent queries will work.
  Fix the daemon first.

- **`stax top` returns `(no samples yet — is a recording in progress?)`** —
  either no run is active, or the run hasn't ingested any PET samples yet
  (very early in the lifecycle). Try `stax status` to confirm a run exists,
  or `stax wait --for-samples 100` to block until data is in.

- **Hardened-runtime targets** still need root *or* a properly entitled
  `staxd`. The unprivileged path works for normal user processes; system
  apps and App Store binaries reject `task_for_pid` regardless of
  entitlements on the caller side.
