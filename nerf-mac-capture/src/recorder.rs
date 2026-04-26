//! High-level recording driver. Acquires a Mach task port for an existing
//! PID, sets up a framehop unwinder seeded with the target's loaded dyld
//! images, and drives a sampling loop until the caller asks to stop.
//!
//! Structure inspired by samply/src/mac/{sampler.rs, task_profiler.rs,
//! thread_profiler.rs} (commit 1920bd32c569de5650d1129eb035f43bd28ace27),
//! but rewritten to emit `SampleSink` events instead of populating a
//! Firefox-profile data model. MIT OR Apache-2.0; see LICENSE-MIT and
//! LICENSE-APACHE at the crate root.

use std::mem;
use std::time::Duration;

use framehop::{CacheNative, FrameAddress, MayAllocateDuringUnwind, UnwinderNative};
use mach2::kern_return::KERN_SUCCESS;
use mach2::mach_types::thread_act_array_t;
use mach2::message::mach_msg_type_number_t;
use mach2::port::{mach_port_t, MACH_PORT_NULL};
use mach2::task::task_threads;
use mach2::traps::{mach_task_self, task_for_pid};
use mach2::vm::mach_vm_deallocate;

use crate::error::SamplingError;
use crate::kernel_error::IntoResult;
use crate::proc_maps::{
    get_backtrace, DyldInfo, DyldInfoManager, ForeignMemory, Modification, StackwalkerRef,
};
use crate::sample_sink::{
    BinaryLoadedEvent, BinaryUnloadedEvent, SampleEvent, SampleSink, ThreadNameEvent,
};
use crate::thread_act::thread_info as mach_thread_info;
use crate::thread_info::{
    thread_extended_info, thread_identifier_info_data_t, THREAD_EXTENDED_INFO,
    THREAD_EXTENDED_INFO_COUNT, THREAD_IDENTIFIER_INFO, THREAD_IDENTIFIER_INFO_COUNT,
};
use crate::time::get_monotonic_timestamp;
use crate::types::UnwindSectionBytes;
use crate::unwinder_setup::{add_lib_to_unwinder, remove_lib_from_unwinder};

/// Configuration for a recording session.
pub struct RecordOptions {
    /// PID to attach to.
    pub pid: u32,
    /// Sampling frequency in Hz.
    pub frequency_hz: u32,
    /// If `Some`, stop recording after this duration. Otherwise run until
    /// `should_stop` flips.
    pub duration: Option<Duration>,
    /// Whether to fold tail-recursive prefixes in unwound stacks.
    pub fold_recursive_prefix: bool,
}

impl Default for RecordOptions {
    fn default() -> Self {
        Self {
            pid: 0,
            frequency_hz: 1000,
            duration: None,
            fold_recursive_prefix: false,
        }
    }
}

/// Drive a recording session against an existing PID. Blocks until the
/// duration elapses, the task disappears, or `should_stop` returns true.
pub fn record<S: SampleSink>(
    opts: RecordOptions,
    sink: &mut S,
    should_stop: impl FnMut() -> bool,
) -> Result<(), SamplingError> {
    let task = task_for_pid_existing(opts.pid)?;
    record_with_task(task, opts, sink, should_stop)
}

/// Same as [`record`], but the caller has already acquired a task port
/// (e.g. via the child-launch / `TaskAccepter` flow).
pub fn record_with_task<S: SampleSink>(
    task: mach_port_t,
    opts: RecordOptions,
    sink: &mut S,
    should_stop: impl FnMut() -> bool,
) -> Result<(), SamplingError> {
    record_with_task_and_tick_hook(task, opts, sink, should_stop, |_| {})
}

/// Same as [`record_with_task`] but invokes `on_tick` at the top of every
/// sampling tick. The hook is handed `&mut S` so it can emit additional
/// events (e.g. Jitdump paths discovered via the IPC accepter).
pub fn record_with_task_and_tick_hook<S: SampleSink>(
    task: mach_port_t,
    opts: RecordOptions,
    sink: &mut S,
    mut should_stop: impl FnMut() -> bool,
    mut on_tick: impl FnMut(&mut S),
) -> Result<(), SamplingError> {
    let mut dyld_manager = DyldInfoManager::new(task);
    let mut unwinder: UnwinderNative<UnwindSectionBytes, MayAllocateDuringUnwind> =
        UnwinderNative::new();
    let mut cache: CacheNative<MayAllocateDuringUnwind> = CacheNative::new();
    let mut foreign_memory = ForeignMemory::new(task);

    // Seed the unwinder with the currently-loaded images.
    apply_dyld_changes(&mut dyld_manager, &mut unwinder, task, opts.pid, sink)?;

    let interval = Duration::from_micros((1_000_000 / opts.frequency_hz.max(1)) as u64);
    let start = std::time::Instant::now();
    let mut next_tick = std::time::Instant::now();
    let mut known_threads: ThreadNameCache = ThreadNameCache::new();
    let mut frames_buf: Vec<FrameAddress> = Vec::with_capacity(256);

    loop {
        if should_stop() {
            break;
        }
        if let Some(dur) = opts.duration {
            if start.elapsed() >= dur {
                break;
            }
        }

        // Caller-provided per-tick hook (e.g. drain a Mach IPC accepter
        // for jitdump-path messages from the preload dylib).
        on_tick(sink);

        // Pick up dyld load/unload events.
        if let Err(err) = apply_dyld_changes(&mut dyld_manager, &mut unwinder, task, opts.pid, sink)
        {
            log::debug!("dyld scan failed: {err}");
        }

        // Sample every thread.
        match sample_all_threads(
            task,
            &unwinder,
            &mut cache,
            &mut foreign_memory,
            &mut frames_buf,
            opts.fold_recursive_prefix,
            opts.pid,
            sink,
            &mut known_threads,
        ) {
            Ok(()) => {}
            Err(err) => {
                // ProcessTerminated means the target died -- exit cleanly.
                if matches!(err, SamplingError::ProcessTerminated(..)) {
                    break;
                }
                log::debug!("sampling tick failed: {err}");
            }
        }

        next_tick += interval;
        let now = std::time::Instant::now();
        if next_tick > now {
            std::thread::sleep(next_tick - now);
        } else {
            // We are behind schedule; reset to "now" instead of letting the
            // backlog grow without bound.
            next_tick = now;
        }
    }

    Ok(())
}

fn task_for_pid_existing(pid: u32) -> Result<mach_port_t, SamplingError> {
    let mut task: mach_port_t = MACH_PORT_NULL;
    let kr = unsafe { task_for_pid(mach_task_self(), pid as i32, &mut task) };
    if kr != KERN_SUCCESS {
        return Err(SamplingError::Fatal(
            "task_for_pid",
            crate::kernel_error::KernelError::from(kr),
        ));
    }
    Ok(task)
}

fn apply_dyld_changes<S: SampleSink>(
    dyld: &mut DyldInfoManager,
    unwinder: &mut UnwinderNative<UnwindSectionBytes, MayAllocateDuringUnwind>,
    task: mach_port_t,
    pid: u32,
    sink: &mut S,
) -> Result<(), SamplingError> {
    let changes = dyld
        .check_for_changes()
        .map_err(|err| SamplingError::Ignorable("DyldInfoManager::check_for_changes", err))?;
    for change in changes {
        match change {
            Modification::Added(lib) => {
                add_lib_to_unwinder(unwinder, task, &lib);
                emit_binary_loaded(pid, &lib, sink);
            }
            Modification::Removed(lib) => {
                remove_lib_from_unwinder(unwinder, lib.base_avma);
                sink.on_binary_unloaded(BinaryUnloadedEvent {
                    pid,
                    base_avma: lib.base_avma,
                    path: &lib.file,
                });
            }
        }
    }
    Ok(())
}

fn emit_binary_loaded<S: SampleSink>(pid: u32, lib: &DyldInfo, sink: &mut S) {
    sink.on_binary_loaded(BinaryLoadedEvent {
        pid,
        base_avma: lib.base_avma,
        vmsize: lib.vmsize,
        text_svma: lib.module_info.base_svma,
        path: &lib.file,
        uuid: lib.uuid,
        arch: lib.arch,
        is_executable: lib.is_executable,
        symbols: &lib.symbols,
    });
}

#[allow(clippy::too_many_arguments)]
fn sample_all_threads<S: SampleSink>(
    task: mach_port_t,
    unwinder: &UnwinderNative<UnwindSectionBytes, MayAllocateDuringUnwind>,
    cache: &mut CacheNative<MayAllocateDuringUnwind>,
    foreign_memory: &mut ForeignMemory,
    frames_buf: &mut Vec<FrameAddress>,
    fold_recursive_prefix: bool,
    pid: u32,
    sink: &mut S,
    known_threads: &mut ThreadNameCache,
) -> Result<(), SamplingError> {
    let timestamp_ns = get_monotonic_timestamp();
    let threads = ThreadList::for_task(task)?;
    for &thread_act in threads.as_slice() {
        // Look up the per-thread tid + name.
        let (tid, thread_name) = match get_thread_id_and_name(thread_act) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if let Some(name) = thread_name {
            if known_threads.note_thread(tid, &name) {
                sink.on_thread_name(ThreadNameEvent {
                    pid,
                    tid,
                    name: &name,
                });
            }
        }

        frames_buf.clear();
        let stackwalker = StackwalkerRef::new(unwinder, cache);
        if let Err(err) = get_backtrace(
            stackwalker,
            foreign_memory,
            thread_act,
            frames_buf,
            fold_recursive_prefix,
        ) {
            // Ignore per-thread failures (terminated threads, etc.); keep going.
            log::trace!("get_backtrace for tid {tid} failed: {err}");
            continue;
        }

        let backtrace: Vec<u64> = frames_buf
            .iter()
            .map(|f| match f {
                FrameAddress::InstructionPointer(addr) => *addr,
                FrameAddress::ReturnAddress(addr) => addr.get(),
            })
            .collect();

        sink.on_sample(SampleEvent {
            timestamp_ns,
            pid,
            tid,
            backtrace: &backtrace,
            kernel_backtrace: &[],
        });
    }
    Ok(())
}

/// Owning wrapper for `task_threads`'s allocated array; deallocates on drop.
struct ThreadList {
    ptr: thread_act_array_t,
    len: mach_msg_type_number_t,
}

impl ThreadList {
    fn for_task(task: mach_port_t) -> Result<Self, SamplingError> {
        let mut ptr: thread_act_array_t = std::ptr::null_mut();
        let mut len: mach_msg_type_number_t = 0;
        unsafe { task_threads(task, &mut ptr, &mut len) }
            .into_result()
            .map_err(|err| match err {
                crate::kernel_error::KernelError::InvalidArgument
                | crate::kernel_error::KernelError::MachSendInvalidDest
                | crate::kernel_error::KernelError::Terminated => {
                    SamplingError::ProcessTerminated("task_threads", err)
                }
                err => SamplingError::Ignorable("task_threads", err),
            })?;
        Ok(Self { ptr, len })
    }

    fn as_slice(&self) -> &[mach_port_t] {
        if self.ptr.is_null() || self.len == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(self.ptr, self.len as usize) }
        }
    }
}

impl Drop for ThreadList {
    fn drop(&mut self) {
        if self.ptr.is_null() {
            return;
        }
        // Release each per-thread port reference. samply's TaskProfiler
        // keeps the ports alive across ticks; we re-acquire them every tick
        // so we must deallocate here to avoid steadily leaking thread-port
        // rights.
        unsafe {
            for &port in self.as_slice() {
                let _ = mach2::mach_port::mach_port_deallocate(mach_task_self(), port);
            }
            let bytes = self.len as u64 * std::mem::size_of::<mach_port_t>() as u64;
            let _ = mach_vm_deallocate(mach_task_self(), self.ptr as u64, bytes);
        }
    }
}

/// Look up the kernel-stable thread id (matches kperf's `arg5`) and
/// the pthread-set name for a `mach_port_t` thread port. Returns
/// `(tid, name)` where name is `None` if `pthread_setname_np` was
/// never called or the read failed.
pub fn get_thread_id_and_name(
    thread_act: mach_port_t,
) -> crate::kernel_error::Result<(u32, Option<String>)> {
    // THREAD_IDENTIFIER_INFO -> stable thread_id (matches `gettid`-ish semantics).
    let mut id_info: thread_identifier_info_data_t = unsafe { mem::zeroed() };
    let mut count = THREAD_IDENTIFIER_INFO_COUNT;
    unsafe {
        mach_thread_info(
            thread_act,
            THREAD_IDENTIFIER_INFO,
            &mut id_info as *mut thread_identifier_info_data_t as *mut _,
            &mut count,
        )
    }
    .into_result()?;
    let tid = id_info.thread_id as u32;

    // THREAD_EXTENDED_INFO -> pthread_setname_np()-set name, if any.
    let mut ext_info: thread_extended_info = unsafe { mem::zeroed() };
    let mut count = THREAD_EXTENDED_INFO_COUNT;
    let name = match unsafe {
        mach_thread_info(
            thread_act,
            THREAD_EXTENDED_INFO,
            &mut ext_info as *mut thread_extended_info as *mut _,
            &mut count,
        )
    }
    .into_result()
    {
        Ok(()) => {
            let bytes: Vec<u8> = ext_info
                .pth_name
                .iter()
                .take_while(|&&c| c != 0)
                .map(|&c| c as u8)
                .collect();
            if bytes.is_empty() {
                None
            } else {
                Some(String::from_utf8_lossy(&bytes).into_owned())
            }
        }
        Err(_) => None,
    };

    Ok((tid, name))
}

/// Tracks which (tid, name) pairs we've already reported so the
/// recorder only emits a `ThreadNameEvent` when the binding actually
/// changes (or appears for the first time).
pub struct ThreadNameCache {
    seen: std::collections::HashMap<u32, String>,
}

impl ThreadNameCache {
    pub fn new() -> Self {
        Self {
            seen: std::collections::HashMap::new(),
        }
    }

    /// Returns true iff the thread name was newly seen or has changed.
    pub fn note_thread(&mut self, tid: u32, name: &str) -> bool {
        match self.seen.get(&tid) {
            Some(existing) if existing == name => false,
            _ => {
                self.seen.insert(tid, name.to_owned());
                true
            }
        }
    }
}

impl Default for ThreadNameCache {
    fn default() -> Self {
        Self::new()
    }
}
