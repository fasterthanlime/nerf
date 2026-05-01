# macOS support roadmap

Goal: enable stax to record on macOS by reusing samply's Mach-based capture
backend. Recorded files feed the existing stax analysis pipeline
(`cmd_collate`, `cmd_flamegraph`, `cmd_csv`, `cmd_trace_events`, …).

Strategy: hybrid. Online mode (unwind-at-sample-time, framehop) ships first,
offline mode (raw stack + regs) follows. No backwards compatibility with v1
stax files — the format break is acknowledged.

## Background — what's reusable from samply

Samply: <https://github.com/mstange/samply>, MIT OR Apache-2.0.

Capture lives in `samply/src/mac/`. It is binary-only (no `lib.rs`), tightly
coupled to `fxprof-processed-profile` and samply's `UnresolvedSamples` types.
We vendor the Mach plumbing and strip the fxprof coupling.

Public crates we depend on directly: `framehop`, `wholesym`, `mach2`.

Vendored files (MIT/Apache headers preserved):
- `proc_maps.rs` — `DyldInfoManager`, `ForeignMemory`, `get_unwinding_registers`,
  `get_backtrace`
- `process_launcher.rs`, `mach_ipc.rs` — `task_for_pid` and the
  `DYLD_INSERT_LIBRARIES` bootstrap for launched children
- `sampler.rs`, `task_profiler.rs`, `thread_profiler.rs`
- `dyld_bindings.rs`, `thread_act.rs`, `thread_info.rs`, `time.rs`,
  `kernel_error.rs`
- `samply-mac-preload` dylib (separate workspace member, becomes
  `stax-mac-preload`)

## Out of scope (for now)

- **Kernel backtraces on macOS.** No public equivalent of
  `PERF_SAMPLE_CALLCHAIN`. Path forward when we want it: `kperf` / `kdebug`
  private frameworks (`libkperf.dylib`, `libktrace.dylib`) with
  reverse-engineered bindings. Requires root or
  `com.apple.private.kernel.system_information` entitlement. Self-contained
  follow-up. Until then, `kernel_backtrace` is empty on mac samples.
- **`cmd_annotate` on aarch64.** Still x86-only via `yaxpeax-x86`.
  Independent of this work.

## M0 — Format break

Small, isolated, verifiable.

- Bump `ARCHIVE_VERSION` 1 → 2 in `src/archive.rs:49`. Magic stays `FRPN`.
- Refuse to read v1 files. No compat shim.
- Replace ELF-baked fields in `BinaryInfo` with a tagged enum:
  ```
  BinaryFormat::Elf  { is_shared_object, debuglink, load_headers }
  BinaryFormat::MachO { uuid, cputype, segments }
  ```
- Split `SymbolTable` → `ElfSymbolTable` / `MachOSymbolTable`. The on-disk
  symbol formats are too different to merge cleanly (ELF SHT_SYMTAB vs.
  Mach-O LC_SYMTAB + nlist + string table).
- Add `Platform` field to `MachineInfo` (`Linux | MacOS`) so `data_reader.rs`
  can dispatch.

Stop and review before M1.

## M1 — Workspace skeleton (done)

Crates compile, no integration yet.

- ✅ New crate `stax-mac-capture`, gated on `#[cfg(target_os = "macos")]`.
- ✅ New crate `stax-mac-preload` (standalone workspace, since it needs
  `no_std` + `panic = "abort"` settings that don't work as a workspace
  member). Vendored verbatim from samply with the bootstrap env var
  renamed `SAMPLY_BOOTSTRAP_SERVER_NAME` → `NERF_BOOTSTRAP_SERVER_NAME`.
- ✅ Leaf-level Mach utilities vendored into `stax-mac-capture`:
  `dyld_bindings`, `kernel_error`, `thread_act`, `thread_info`, `time`,
  `error`, `mach_ipc`. One behaviour change: `mach_ipc::nonce_i64`
  replaces samply's `rand::rng().random::<i64>()` (rand 0.10 has a
  pre-release-only chacha20 transitive dep).
- Dependencies wired up: `mach2 = "0.6"`, `libc`, `framehop = "0.16"`,
  `lazy_static`, `crossbeam-channel`, `thiserror`. samply commit pinned
  in `stax-mac-capture/Cargo.toml`.
- ⏭ Folded into M2: vendoring + stripping `proc_maps`, `process_launcher`,
  `sampler`, `task_profiler`, `thread_profiler`, `profiler`. These files
  carry heavy `fxprof-processed-profile` / `wholesym` / `samply-symbols` /
  `crate::shared::*` coupling. The same surgery that strips that coupling
  also rewires the output to the stax packet writer, so it makes more
  sense to do it in M2 than to land an intermediate stubbed-out state.

## M2 — Online recording

Mac records, analysis pipeline reads the result.

- Vendor + strip the heavy-coupling samply files (`proc_maps`,
  `process_launcher`, `sampler`, `task_profiler`, `thread_profiler`,
  `profiler`). Replace `fxprof-processed-profile` / `UnresolvedSamples` /
  `crate::shared::*` glue with a small `Sample` callback trait that
  emits `Packet::Sample` directly into the stax writer.
- `cmd_record.rs` dispatches by platform: linux → existing path, mac →
  `stax-mac-capture`.
- Mac path: framehop unwinds at sample time. Each sample becomes
  `Packet::Sample { user_backtrace: Vec<UserFrame { address, symbol_id: None,
  is_inline: false }>, kernel_backtrace: empty, … }`.
- On dyld load events from `DyldInfoManager`, emit `Packet::BinaryInfo`
  (Mach-O variant) + `Packet::MachOSymbolTable` (LC_SYMTAB + string table).
- Symbol resolution: do what stax does — leave `symbol_id: None` at record
  time; `data_reader.rs` resolves from the embedded `MachOSymbolTable` at
  load time. No record-time wholesym calls.
- Verify: `cmd_metadata` reads the file. `cmd_flamegraph` produces a sensible
  SVG against a real recorded mac process.

Stop and review before M3.

## M3a — Child-launch + preload-dylib + JIT auto-discovery

Brings up the `stax record --process <cmd>` path on macOS so that:

- We can spawn a target with `DYLD_INSERT_LIBRARIES` pointing at our
  preload dylib, which Mach-IPC's its task port back to us. This is
  symmetric with samply's existing flow.
- The preload dylib's hooked `open()` / `fopen()` reports
  `jit-<pid>.dump` paths back over the same channel, so JIT runtimes
  (Cranelift / wasmtime / V8 / JVM) get auto-discovered without the
  user having to pass `--jitdump <path>` to `collate`.

Steps:

1. Build pipeline: `stax-mac-capture/build.rs` invokes `cargo build`
   on `stax-mac-preload` for the host target, gzips the resulting
   cdylib, places the bytes in `$OUT_DIR/libstax_mac_preload.dylib.gz`.
   `stax-mac-capture` then `include_bytes!`s the blob and drops it to
   a tempfile at runtime. (Multi-arch / `lipo` come later; host-only
   for v1.)
2. Vendor + strip `samply/src/mac/process_launcher.rs` into
   `stax-mac-capture/src/process_launcher.rs`. Replace
   `crate::shared::ctrl_c::CtrlC` with a small inline equivalent.
   Drop the `__XPC_*` env-var double-set unless we actually need it.
3. Surface `JitdumpPath` events on `SampleSink`. The on-disk
   embedding of the jitdump file (so `collate` can find it without
   `--jitdump`) goes via a `FileBlob` packet keyed off the path.
4. Teach `cmd_collate` to recognise embedded jitdump `FileBlob`s
   (alternative path to the existing `--jitdump` flag).
5. Wire `cmd_record_mac.rs` to dispatch `--process` to a new
   child-launch path that uses `TaskLauncher` + `TaskAccepter`.

## M3b — Hybrid: offline mode on mac

Raw-stack capture for deferred unwinding. Independent of M3a.

- Capture raw stack pages via `mach_vm_read_overwrite` (SP region, ~256 KiB)
  plus `ARM_THREAD_STATE64` / `x86_THREAD_STATE64` registers. Emit
  `Packet::RawSample`.
- Unwinding raw mac samples in `data_reader.rs`: dispatch to framehop with
  a reader that pulls `__unwind_info` / `__eh_frame` from `BinaryBlob`-stored
  Mach-O bytes.

Stop and review.
