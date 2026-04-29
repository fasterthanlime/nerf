# Stream Delivery And Backpressure Roadmap

## Problem

`staxd`, `stax-shade`, `stax-server`, and the CLI currently mix three different
communication shapes without naming their semantics:

- reliable lifecycle/control transitions,
- high-rate telemetry streams,
- latest-value UI streams.

The result is accidental buffering. The latest observed hang was not `staxd`
failing to stop. `staxd` stopped and tore down kperf/kdebug cleanly, but
`stax-shade` was still waiting for the local parser worker to drain a private
unbounded queue of kdebug batches.

That is the core rule violation:

```text
bounded stream -> unbounded local queue -> slow parser/ingest worker
```

The receiver from `staxd` was draining. The parser queue was not.

## Non-Goals

- Do not add `Tx::send_timeout` to vox for this. Callers can already wrap
  `send` in `tokio::time::timeout`.
- Do not make the CLI supervise `stax-shade`.
- Do not rely on channels for authoritative lifecycle success.
- Do not silently drop correctness-critical events.

## Existing Vox Semantics

Vox channels already apply backpressure, currently with a hardcoded capacity of
16. That is good enough as a transport primitive for now.

The missing pieces are:

- configurable capacity,
- `try_send` / `Full` reporting for callers that want nonblocking policy,
- explicit metrics so backpressure is visible.

`try_send` is useful because some streams should not await receiver progress:
UI updates, logs, and latest-value streams should drop or coalesce stale data.
For reliable streams, normal `send().await` is still the right default.

## Required Invariants

1. No component may turn a bounded stream into an unbounded private queue.
2. The kdebug receive loop in shade must stay a critical fast path.
3. Probe triggering must not wait behind parsing, image scanning, symbol lookup,
   ingest forwarding, or UI work.
4. All lossy policies must be explicit and surfaced.
5. Lifecycle transitions must be method calls with acknowledgements.
6. Shutdown must choose between flushing and dropping backlog explicitly.

## Stream Classes

### Reliable Method Calls

Use RPC methods. Do not model these as channel messages.

Examples:

- `start_run`
- `register_shade`
- `recording_ready`
- `target_attached`
- `target_exited`
- `recording_finished`
- final `flush` / `finish` acknowledgements
- possibly `binary_loaded`, if we decide missing it invalidates symbolication

Properties:

- caller gets success/failure,
- retry/idempotency can be defined,
- failures are not hidden as channel closure or backlog.

### Reliable Streams

Use bounded channels with normal backpressure and measured queueing.

Examples:

- `staxd -> shade` raw kdebug batches while recording,
- shade ingest stream for sample events if loss is not acceptable.

Properties:

- bounded capacity,
- queue depth/age metrics,
- sender awaiting is allowed only when backpressure is intended.

### Latest-Value / Best-Effort Streams

Use bounded `try_send` or coalescing queues.

Examples:

- live flamegraph updates,
- live thread updates,
- logs,
- UI progress updates,
- probe-diff subscription updates.

Properties:

- stale updates are dropped or replaced,
- receiver never causes producer runaway,
- data loss is acceptable by design.

## Immediate Priority: Split The Critical Fast Path

This should happen before broader API work.

Desired shade receive path:

```text
recv kdebug batch from staxd
scan raw records for kperf sample start / user stack header
trigger race probe immediately
enqueue parser work using bounded explicit policy
return to recv
```

Forbidden on this path:

- `Pipeline::process_records`,
- `Pipeline::tick`,
- image scanning,
- Mach-O parsing,
- symbol lookup,
- demangling,
- ingest flushing,
- waiting for parser backlog.

The raw kperf scanner belongs directly in the receive loop. The parser worker
can lag, drop, or shut down without delaying probe capture.

## Parser Queue Policy

Replace the local unbounded `std::sync::mpsc::channel` with a bounded queue.

Candidate crates:

- `flume`: good default. Supports bounded channels, `try_send`, sync receive,
  async receive, `len`, and works from both OS threads and async tasks.
- `crossbeam-channel`: also good for sync worker threads, but less convenient if
  we later want async receive.
- `tokio::sync::mpsc`: good on async tasks, less natural for the dedicated sync
  parser OS thread.

Recommendation: use `flume::bounded`.

Policy:

```rust
enum ParserQueuePolicy {
    ActiveDropOldest,
    ShutdownDropBacklog,
}
```

Active recording:

- never block the kdebug recv loop,
- enqueue chunks, not giant batches,
- if full, drop oldest parser chunks and enqueue newest,
- increment loss counters.

Shutdown:

- stop accepting new parser work,
- choose `FlushUntil(deadline)` or `DropBacklog`,
- always return promptly after the chosen policy completes.

Important: dropping parser chunks during active recording loses parsed samples.
That is acceptable only if surfaced loudly in the run metadata and UI.

## Loss Visibility

Every lossy edge needs counters.

Minimum run stats:

- `parser_queue_capacity`
- `parser_queue_depth_max`
- `parser_queue_age_max_ns`
- `parser_dropped_batches`
- `parser_dropped_records`
- `parser_dropped_kperf_samples`
- `probe_requests_enqueued`
- `probe_requests_coalesced`
- `probe_results_emitted`
- `probe_results_dropped`
- `ui_updates_dropped`

Surface these in:

- unified logs,
- `stax status` / `stax list` summary when nonzero,
- web UI run diagnostics,
- `stax probe-diff` header.

If `parser_dropped_records > 0`, the profile is partial. Say so.

## Shutdown Semantics

Replace implicit worker drain with explicit stop modes:

```rust
enum StopMode {
    FlushUntil(Duration),
    DropBacklog,
}
```

Recommended behavior:

- normal target exit: `FlushUntil(short_deadline)`, then `DropBacklog`,
- Ctrl-C / user stop: `DropBacklog` after stopping `staxd`,
- tests may request `FlushUntil(long_deadline)` to verify no loss.

The parser worker must not keep the run active indefinitely because it owns the
sink. Dropping backlog must close ingest deliberately and let shade exit.

## Vox Work

After the local fast-path split is correct:

1. Make channel capacity configurable instead of hardcoded `16`.
2. Add `Tx::try_send`.
3. Return distinct errors:

   ```rust
   enum TrySendError<T> {
       Full(T),
       Closed(T),
   }
   ```

4. Add channel instrumentation:

   - sends attempted,
   - sends awaited,
   - try-send full count,
   - close reason,
   - max in-flight capacity use if available.

Do not add `send_timeout` unless it provides something materially better than
`tokio::time::timeout(tx.send(value), duration)`.

## Server Subscription Policy

Subscriptions are latest-value streams unless explicitly documented otherwise.

Use bounded/coalescing output for:

- `subscribe_top`,
- `subscribe_flamegraph`,
- `subscribe_threads`,
- `subscribe_timeline`,
- `subscribe_probe_diff`,
- annotation/disassembly live updates.

Slow browser clients must not cause `stax-server` CPU runaway or memory growth.

## Ordered Work Plan

1. **Critical fast path split**
   Ensure shade's kdebug recv loop does only raw scan + probe trigger + bounded
   parser enqueue.

2. **Bound parser queue**
   Replace local unbounded mpsc with `flume::bounded`, chunk records, implement
   drop-oldest-on-full for parser work.

3. **Stop semantics**
   Add `StopMode`, make Ctrl-C/target-exit use explicit drop/flush behavior,
   remove any "wait forever for parser worker" path.

4. **Loss counters**
   Add parser/probe/drop counters to run diagnostics and ingest/server state.

5. **Surface diagnostics**
   Show nonzero loss/backlog stats in logs, CLI, `probe-diff`, and web UI.

6. **Vox `try_send`**
   Add nonblocking send and configurable capacity to vox channels.

7. **Reliable lifecycle RPCs**
   Move any correctness-critical channel event to method-call/ack semantics.

8. **Subscription coalescing**
   Make all live UI streams latest-value bounded streams.

9. **Remove temporary safety valves**
   Once bounded queues and explicit stop modes are in place, remove or demote
   worker-detach timeout workarounds to last-resort diagnostics.
