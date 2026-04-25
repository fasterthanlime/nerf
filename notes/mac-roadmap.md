# macOS support roadmap

Goal: enable nerf to record on macOS by reusing samply's Mach-based capture
backend. Recorded files feed the existing nperf analysis pipeline
(`cmd_collate`, `cmd_flamegraph`, `cmd_csv`, `cmd_trace_events`, …).

Strategy: hybrid. Online mode (unwind-at-sample-time, framehop) ships first,
offline mode (raw stack + regs) follows. No backwards compatibility with v1
nperf files — the format break is acknowledged.

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
  `nerf-mac-preload`)

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

## M1 — Workspace skeleton

Crates compile, no integration yet.

- New crate `nerf-mac-capture`, gated on `#[cfg(target_os = "macos")]`
  everywhere it's referenced from the workspace.
- New crate `nerf-mac-preload` (the `DYLD_INSERT_LIBRARIES` shim).
- Dependencies:
  - git-dep: `framehop`, `wholesym`
  - crates.io: `mach2`
  - pin samply's revision in a comment in `Cargo.toml` for traceability
- Vendor the files listed above. Replace `fxprof-processed-profile` /
  `UnresolvedSamples` coupling with a small `Sample` callback trait owned by
  `nerf-mac-capture`.

Stop and review before M2.

## M2 — Online recording

Mac records, analysis pipeline reads the result.

- `cmd_record.rs` dispatches by platform: linux → existing path, mac →
  `nerf-mac-capture`.
- Mac path: framehop unwinds at sample time. Each sample becomes
  `Packet::Sample { user_backtrace: Vec<UserFrame { address, symbol_id: None,
  is_inline: false }>, kernel_backtrace: empty, … }`.
- On dyld load events from `DyldInfoManager`, emit `Packet::BinaryInfo`
  (Mach-O variant) + `Packet::MachOSymbolTable` (LC_SYMTAB + string table).
- Symbol resolution: do what nperf does — leave `symbol_id: None` at record
  time; `data_reader.rs` resolves from the embedded `MachOSymbolTable` at
  load time. No record-time wholesym calls.
- Verify: `cmd_metadata` reads the file. `cmd_flamegraph` produces a sensible
  SVG against a real recorded mac process.

Stop and review before M3.

## M3 — Hybrid: offline mode on mac

Raw-stack capture for deferred unwinding.

- Capture raw stack pages via `mach_vm_read_overwrite` (SP region, ~256 KiB)
  plus `ARM_THREAD_STATE64` / `x86_THREAD_STATE64` registers. Emit
  `Packet::RawSample`.
- Unwinding raw mac samples in `data_reader.rs`: dispatch to framehop with
  a reader that pulls `__unwind_info` / `__eh_frame` from `BinaryBlob`-stored
  Mach-O bytes. Do **not** teach `nwind` Mach-O. nwind stays ELF/DWARF.

Stop and review.
