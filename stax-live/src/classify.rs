//! Off-CPU classifier: turn the leaf user-space frame at the moment
//! a thread blocked into an `OffCpuReason`.
//!
//! Every off-CPU interval has a calling card: the leaf user PC at
//! the moment the thread parked. `__psynch_cvwait` means cond-var
//! wait, `__psynch_mutexwait` means contention, `read` means IO,
//! and so on. Knowing *why* a thread blocked is the difference
//! between "boring scheduler noise" and "this is the bottleneck."
//!
//! The matcher is pattern-based on the demangled symbol name, which
//! we already resolve through the binary registry. Symbols we don't
//! recognise (or off-CPU intervals with no PET stack to look up)
//! land in `OffCpuReason::Other`; that bucket is the "needs more
//! taxonomy" signal.

use stax_live_proto::OffCpuReason;

/// Classify an off-CPU interval from the leaf-frame symbol name.
///
/// `function_name` is what `BinaryRegistry::lookup_symbol` returned
/// for the leaf address (already demangled). `None` here means the
/// frame couldn't be resolved at all; we still try a few patterns
/// against the empty string (always fall through to `Other`).
pub fn classify_offcpu(function_name: Option<&str>) -> OffCpuReason {
    let Some(name) = function_name else {
        return OffCpuReason::Other;
    };

    // Pattern matches are ordered by specificity: more-specific
    // pthread / kqueue functions before broad fallbacks. The
    // matchers all use `starts_with` / `==` rather than `contains`
    // because Rust's mangling sometimes embeds these names as
    // substrings (e.g. `<some::wrapper as Trait>::write`) and we
    // don't want a Rust function named "writer_loop" classified as
    // an IO syscall.

    // -- pthread / ulock synchronisation primitives --------------------
    // pthread_cond_wait & friends; the syscall stub is
    // `__psynch_cvwait`. ulock_wait is the libsystem-internal
    // futex-style primitive used by os_unfair_lock and dispatch.
    if name == "__psynch_cvwait"
        || name == "__ulock_wait"
        || name == "__ulock_wait2"
        || name == "__workq_kernreturn"
        || name == "_pthread_cond_wait"
        || name == "_pthread_cond_timedwait"
        || name == "_dispatch_workloop_worker_thread"
    {
        return OffCpuReason::Idle;
    }
    // Mutex / rwlock contention (lock owned by someone else; thread
    // wants to run but has to wait for the holder). This is the
    // off-CPU you usually want to chase down.
    if name == "__psynch_mutexwait"
        || name == "__psynch_rw_rdlock"
        || name == "__psynch_rw_wrlock"
        || name == "__psynch_rw_yieldwrlock"
        || name == "__psynch_rw_upgrade"
        || name == "__psynch_rw_downgrade"
        || name == "_pthread_mutex_firstfit_lock_wait"
        || name == "_pthread_mutex_lock"
        || name == "_pthread_mutex_lock_wait"
    {
        return OffCpuReason::LockWait;
    }

    // -- Semaphores ----------------------------------------------------
    if name == "__semwait_signal"
        || name == "__semwait_signal_nocancel"
        || name == "semaphore_wait_trap"
        || name == "semaphore_timedwait_trap"
        || name == "_dispatch_semaphore_wait"
    {
        return OffCpuReason::SemaphoreWait;
    }

    // -- Mach IPC ------------------------------------------------------
    // Threads blocked here are typically waiting for a Mach reply
    // port, which is either RPC or a dispatch-source delivery.
    if name == "mach_msg2_trap"
        || name == "mach_msg_trap"
        || name == "mach_msg_overwrite_trap"
        || name == "mach_msg2"
        || name == "mach_msg"
        || name == "mach_msg_overwrite"
    {
        return OffCpuReason::IpcWait;
    }

    // -- fd readiness --------------------------------------------------
    // Order matters: kevent goes first so kqueue waits don't fall
    // into the IO bucket.
    if name == "kevent"
        || name == "kevent_id"
        || name == "kevent_qos"
        || name == "select"
        || name == "select$DARWIN_EXTSN"
        || name == "select$DARWIN_EXTSN$NOCANCEL"
        || name == "pselect"
        || name == "poll"
        || name == "ppoll"
    {
        return OffCpuReason::Readiness;
    }

    // -- Explicit sleeps ----------------------------------------------
    if name == "nanosleep"
        || name == "__semwait_signal_nocancel"
        || name == "__nanosleep"
        || name == "usleep"
    {
        return OffCpuReason::Sleep;
    }

    // -- IO reads / writes --------------------------------------------
    // Match the libsystem syscall stubs *and* a few common cancellable
    // variants. We use `==` so user code named e.g. "writer" doesn't
    // get caught.
    if name == "read"
        || name == "__read_nocancel"
        || name == "recv"
        || name == "__recvfrom"
        || name == "recvfrom"
        || name == "__recvfrom_nocancel"
        || name == "recvmsg"
        || name == "__recvmsg_nocancel"
        || name == "pread"
        || name == "__pread_nocancel"
        || name == "readv"
    {
        return OffCpuReason::IoRead;
    }
    if name == "write"
        || name == "__write_nocancel"
        || name == "send"
        || name == "__sendto"
        || name == "sendto"
        || name == "__sendto_nocancel"
        || name == "sendmsg"
        || name == "__sendmsg_nocancel"
        || name == "pwrite"
        || name == "__pwrite_nocancel"
        || name == "writev"
    {
        return OffCpuReason::IoWrite;
    }

    // -- Connection setup ---------------------------------------------
    if name == "connect"
        || name == "__connect_nocancel"
        || name == "accept"
        || name == "__accept_nocancel"
        || name == "open"
        || name == "__open_nocancel"
        || name == "openat"
        || name == "__openat_nocancel"
    {
        return OffCpuReason::ConnectionSetup;
    }

    OffCpuReason::Other
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_paths() {
        assert_eq!(classify_offcpu(Some("__psynch_cvwait")), OffCpuReason::Idle);
        assert_eq!(
            classify_offcpu(Some("__workq_kernreturn")),
            OffCpuReason::Idle
        );
        assert_eq!(classify_offcpu(Some("__ulock_wait")), OffCpuReason::Idle);
    }

    #[test]
    fn lock_contention() {
        assert_eq!(
            classify_offcpu(Some("__psynch_mutexwait")),
            OffCpuReason::LockWait
        );
        assert_eq!(
            classify_offcpu(Some("__psynch_rw_wrlock")),
            OffCpuReason::LockWait
        );
    }

    #[test]
    fn ipc_and_readiness() {
        assert_eq!(
            classify_offcpu(Some("mach_msg2_trap")),
            OffCpuReason::IpcWait
        );
        assert_eq!(classify_offcpu(Some("kevent_id")), OffCpuReason::Readiness);
        assert_eq!(classify_offcpu(Some("poll")), OffCpuReason::Readiness);
    }

    #[test]
    fn io_split() {
        assert_eq!(classify_offcpu(Some("read")), OffCpuReason::IoRead);
        assert_eq!(
            classify_offcpu(Some("__read_nocancel")),
            OffCpuReason::IoRead
        );
        assert_eq!(classify_offcpu(Some("write")), OffCpuReason::IoWrite);
    }

    #[test]
    fn rust_function_named_write_does_not_match() {
        // A Rust function whose demangled name happens to contain
        // "write" must NOT be classified as an IO write -- the
        // matcher uses `==`, not `contains`.
        assert_eq!(
            classify_offcpu(Some("my_crate::Buffer::writer_loop")),
            OffCpuReason::Other
        );
        assert_eq!(classify_offcpu(Some("std::io::write")), OffCpuReason::Other);
    }

    #[test]
    fn no_symbol_is_other() {
        assert_eq!(classify_offcpu(None), OffCpuReason::Other);
        assert_eq!(classify_offcpu(Some("")), OffCpuReason::Other);
    }
}
