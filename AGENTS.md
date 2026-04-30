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
to `~/.cargo/bin/`, codesigns them on macOS, and **bootstraps `stax-server`
as a per-user LaunchAgent** so it's running on login.

On macOS, `cargo xtask install` prefers `Developer ID Application` and then
`Apple Development` identities from `security find-identity`. Override with
`STAX_CODESIGN_IDENTITY=<identity-or-hash>`; set it to `-` only when you
explicitly want ad-hoc signing.

After that, one privileged step installs `staxd` as a LaunchDaemon:

```
sudo -n /usr/local/sbin/stax-agent setup --yes
```

…installs `staxd` (the privileged kperf/kdebug owner) as a LaunchDaemon. From
this point on, `stax record …` is unprivileged. Human operators without the
agent wrapper can run `sudo stax setup` manually.

### Privileged agent commands

On Amos's development machine, agents have a passwordless privileged wrapper:

```
sudo -n /usr/local/sbin/stax-agent setup --yes
sudo -n /usr/local/sbin/stax-agent dump
```

Use `stax-agent` instead of interactive `sudo stax` for privileged stax
operations. The `-n` is intentional: if sudo would ask for a password, fail
fast and ask the user rather than blocking in an interactive prompt.

`log` is also configured for passwordless sudo on this machine. Use:

```
sudo -n log show --last 5m --predicate 'subsystem == "eu.bearcove.staxd"'
sudo -n log stream --predicate 'subsystem == "eu.bearcove.staxd"'
```

### What runs where

| component     | privilege  | launchd kind      | socket                                     |
|---------------|------------|-------------------|--------------------------------------------|
| `staxd`       | root       | LaunchDaemon      | `/var/run/staxd.sock`                      |
| `stax-server` | user       | LaunchAgent       | `$XDG_RUNTIME_DIR/stax-server.sock` or `/tmp/stax-server-$UID.sock` |
| `stax`        | user       | (CLI)             | (no socket)                                |

`stax-server` also binds **`ws://127.0.0.1:8080`** for the web UI. Override
with `STAX_SERVER_WS_BIND=host:port` (set in the LaunchAgent plist's
`EnvironmentVariables` if you want it persistent).

The default local socket intentionally lives outside
`~/Library/Group Containers`. A bare LaunchAgent/CLI touching app data paths
triggers `kTCCServiceSystemPolicyAppData` prompts even when it is signed by
the right team.

### Logs

Both daemons log via macOS unified logging (`os_log`). No files on
disk — view live with:

```
# stax-server (your user, no sudo)
log stream --predicate 'subsystem == "eu.bearcove.stax-server"'

# staxd (root LaunchDaemon — needs sudo)
sudo -n log stream --predicate 'subsystem == "eu.bearcove.staxd"'
```

Or open **Console.app** → Action menu → *Include Info Messages* /
*Include Debug Messages*, then filter by subsystem. Past events are
queryable with `log show --last 10m --predicate '…'`.

### Verifying the install

```
stax status            # talks to stax-server, reports active run
test -S /var/run/staxd.sock              # staxd socket exists
launchctl list eu.bearcove.stax-server  # should show pid + 0
```

## Concurrency model

**One active run at a time.** `stax-server` rejects a second `start_run` while
one is in flight. If you hit this, your options are:

```
stax wait     # block until the active run stops
stax stop     # ask stax-server to stop it now
```

`stax list` shows every run the daemon has hosted (active + history,
in-memory only for now — persistence is a follow-up).

### Which run does `stax top` / `stax annotate` query?

There's no run selector yet. They operate on the **current** aggregator
state, which is whichever run is active *or* the most recent one — the
aggregator stays populated until the next `start_run` resets it. So the
working flow is:

```
stax record …           # start a recording (in another shell or backgrounded)
stax wait --for-samples 5000
stax top                # snapshot of the active run
stax stop               # stops the run; aggregator stays queryable
stax top                # still works — same data as above
stax record …           # NEW run resets the aggregator; the previous one is gone
```

If you need to query an older run later, you'll have to stop the active
one first (so its data sticks around) and avoid starting a new recording
until you're done. Per-`RunId` querying is on the roadmap.

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

### `stax threads [-n N]`

Per-thread on/off-CPU breakdown for the current run, sorted by
on-CPU time descending. Use it to figure out *which thread* is
worth flaming.

```
$ stax threads -n 5
 on-CPU ms off-CPU ms    samples   blocked  tid    name
   1240.20      31.40       1102      lock  501    main
    860.00      99.00        710     sleep  592    tokio-runtime-worker
    220.10      14.50        198      idle  600    grpc-pool
    …
```

The `blocked` column names the largest off-CPU bucket for that
thread (`idle`, `lock`, `sem`, `ipc`, `ioR`, `ioW`, `ready`,
`sleep`, `conn`, `other`).

`-n 0` prints every thread. Default 20.

### `stax flame [-d MAX_DEPTH] [--threshold-pct PCT] [--tid TID]`

Print the on-CPU flamegraph as an indented Markdown tree, sorted by
`on_cpu_ns` descending at each level. Same data the web UI renders;
this is the agent-friendly view of "where is the time going."

- `-d / --max-depth N` — cut off the tree at depth N (default 12).
  Children below the cut-off are summarised as `…N more frames`.
- `--threshold-pct PCT` — hide subtrees whose share of total
  on-CPU falls below `PCT` (default 1%; pass `0` for the whole tree).
- `--tid` — filter to one thread.

Operates on the current run's aggregator (same rules as `stax top`).

```
$ stax flame -d 4 --threshold-pct 2
# stax flame · total on-CPU 2.503s · off-CPU 4.122s

`​``
   2503ms 100.0%  (root)
   1201ms  48.0%    └─ vox_jit::translate  (libvox.dylib)
    901ms  36.0%      └─ cranelift::lower  (libcranelift.dylib)
    402ms  16.0%        └─ cranelift::regalloc  (libcranelift.dylib)
    200ms   8.0%      └─ vox_postcard::deserialize  (libvox.dylib)
    802ms  32.1%    └─ tokio::runtime::poll_task  (libtokio.dylib)
        …18 more frames
`​``
```

### `stax annotate <TARGET> [--tid TID]`

Disassemble + annotate one function from the current run.

`TARGET` is either:
- a **hex address** (`0x10004ad60`) — passed straight through to the
  Profiler RPC.
- a **substring of a function name** (`translate`, `cranelift::lower`,
  `MyType::method`) — case-insensitive. The CLI asks for the top 256
  leaf-self functions and picks the hottest one whose demangled name
  matches; the address that wins gets logged so you can re-target by
  address next time.

If nothing matches, you'll see the hottest names that *did* land —
useful when nothing's been sampled yet, or your symbol got merged into a
parent (try a name from `stax top` directly).

```
$ stax annotate translate
stax: matched "translate" → vox_jit::translate (3812 self samples)
; vox_jit::translate (rust) @ 0x10004ad58
; src/translate.rs:412
  0x10004ad58      0 samples    push rbp
  0x10004ad59      0 samples    mov  rbp, rsp
  0x10004ad5c     14 samples    mov  rax, qword ptr [rsi]
  …
```

`--tid` filters to one thread. Omit for whole-process.

### `stax setup`

Privileged install of `staxd` (LaunchDaemon). Agents should use
`sudo -n /usr/local/sbin/stax-agent setup --yes` on Amos's machine, not
interactive `sudo stax setup`. Not part of the routine agent flow.

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

- `local://$XDG_RUNTIME_DIR/stax-server.sock` or `/tmp/stax-server-$UID.sock`,
  for trusted local agents and the stax macOS app
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

- **macOS asks whether `stax-server` can access another app's data** —
  the server is touching an app/container data path. By default it should use
  `$XDG_RUNTIME_DIR/stax-server.sock` or `/tmp/stax-server-$UID.sock`, not a
  path under `~/Library/Group Containers`.

- **`stax top` returns `(no samples yet — is a recording in progress?)`** —
  either no run is active, or the run hasn't ingested any PET samples yet
  (very early in the lifecycle). Try `stax status` to confirm a run exists,
  or `stax wait --for-samples 100` to block until data is in.

- **Hardened-runtime targets** are out of scope. The attachment helper is
  same-uid and intended for normal local developer processes.
