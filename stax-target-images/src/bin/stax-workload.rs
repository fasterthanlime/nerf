use std::env;
use std::hint::black_box;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

#[derive(Clone, Copy, Debug)]
struct Config {
    seconds: u64,
    threads: usize,
    depth: u32,
    yield_every: u64,
    sleep_us: u64,
    helper: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            seconds: 10,
            threads: thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(2)
                .clamp(1, 2),
            depth: 12,
            yield_every: 256,
            sleep_us: 1_000,
            helper: false,
        }
    }
}

fn main() -> ExitCode {
    let config = match parse_args() {
        Ok(config) => config,
        Err(err) => {
            eprintln!("{err}");
            eprintln!(
                "usage: stax-workload [--seconds N] [--threads N] [--depth N] [--yield-every N] [--sleep-us N] [--helper]"
            );
            return ExitCode::from(2);
        }
    };

    eprintln!(
        "stax-workload: pid={} seconds={} threads={} depth={} yield_every={} sleep_us={} helper={}",
        std::process::id(),
        config.seconds,
        config.threads,
        config.depth,
        config.yield_every,
        config.sleep_us,
        config.helper
    );

    let _helper = if config.helper {
        start_stack_helper()
    } else {
        None
    };

    let stop = Arc::new(AtomicBool::new(false));
    let total_iterations = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::with_capacity(config.threads);

    for worker_id in 0..config.threads {
        let stop = stop.clone();
        let total_iterations = total_iterations.clone();
        handles.push(
            thread::Builder::new()
                .name(format!("stax-workload-{worker_id}"))
                .spawn(move || worker(worker_id as u64, config, stop, total_iterations))
                .expect("spawn worker thread"),
        );
    }

    thread::sleep(Duration::from_secs(config.seconds));
    stop.store(true, Ordering::Relaxed);

    let mut checksum = 0u64;
    for handle in handles {
        checksum ^= handle.join().expect("worker thread panicked");
    }

    println!(
        "stax-workload: done iterations={} checksum={checksum:#x}",
        total_iterations.load(Ordering::Relaxed)
    );
    ExitCode::SUCCESS
}

fn parse_args() -> Result<Config, String> {
    let mut config = Config::default();
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        let value = match arg.as_str() {
            "--seconds" => &mut config.seconds,
            "--threads" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--threads requires a value".to_owned())?;
                config.threads = value
                    .parse::<usize>()
                    .map_err(|e| format!("invalid --threads value {value:?}: {e}"))?
                    .max(1);
                continue;
            }
            "--depth" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--depth requires a value".to_owned())?;
                config.depth = value
                    .parse::<u32>()
                    .map_err(|e| format!("invalid --depth value {value:?}: {e}"))?;
                continue;
            }
            "--yield-every" => &mut config.yield_every,
            "--sleep-us" => &mut config.sleep_us,
            "--helper" => {
                config.helper = true;
                continue;
            }
            "--help" | "-h" => {
                return Err(
                    "stax-workload generates CPU and scheduler activity for stax".to_owned(),
                );
            }
            _ => return Err(format!("unknown argument {arg:?}")),
        };
        let raw = args
            .next()
            .ok_or_else(|| format!("{arg} requires a value"))?;
        *value = raw
            .parse::<u64>()
            .map_err(|e| format!("invalid {arg} value {raw:?}: {e}"))?;
    }
    Ok(config)
}

fn worker(
    worker_id: u64,
    config: Config,
    stop: Arc<AtomicBool>,
    total_iterations: Arc<AtomicU64>,
) -> u64 {
    let mut seed = 0x9e37_79b9_7f4a_7c15 ^ worker_id.rotate_left(17);
    let mut iterations = 0u64;
    while !stop.load(Ordering::Relaxed) {
        seed ^= stack_burn(seed.wrapping_add(iterations), config.depth);
        iterations = iterations.wrapping_add(1);
        if config.yield_every != 0 && iterations % config.yield_every == 0 {
            thread::yield_now();
            if config.sleep_us != 0 {
                thread::sleep(Duration::from_micros(config.sleep_us));
            }
        }
    }
    total_iterations.fetch_add(iterations, Ordering::Relaxed);
    black_box(seed ^ iterations)
}

#[inline(never)]
fn stack_burn(seed: u64, depth: u32) -> u64 {
    if depth == 0 {
        return mix(seed);
    }
    let left = stack_burn(seed.rotate_left(7) ^ u64::from(depth), depth - 1);
    mix(left ^ seed.rotate_right(depth % 63 + 1))
}

#[inline(never)]
fn mix(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

#[cfg(target_os = "macos")]
fn start_stack_helper() -> Option<thread::JoinHandle<()>> {
    use std::os::unix::net::UnixListener;

    let path = helper_socket_path(std::process::id());
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!(
                "stax-workload: helper bind {} failed: {err}",
                path.display()
            );
            return None;
        }
    };
    eprintln!("stax-workload: helper listening at {}", path.display());
    Some(
        thread::Builder::new()
            .name("stax-workload-helper".to_owned())
            .spawn(move || {
                let mut helper = StackHelper::new();
                for stream in listener.incoming() {
                    match stream {
                        Ok(stream) => helper.serve(stream),
                        Err(err) => {
                            eprintln!("stax-workload: helper accept failed: {err}");
                            break;
                        }
                    }
                }
                let _ = std::fs::remove_file(&path);
            })
            .expect("spawn stax workload helper"),
    )
}

#[cfg(not(target_os = "macos"))]
fn start_stack_helper() -> Option<thread::JoinHandle<()>> {
    eprintln!("stax-workload: --helper is only implemented on macOS");
    None
}

#[cfg(target_os = "macos")]
fn helper_socket_path(pid: u32) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("/tmp/stax-inferior-helper-{pid}.sock"))
}

#[cfg(target_os = "macos")]
mod mac_helper {
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    use mach2::kern_return::KERN_SUCCESS;
    use mach2::mach_port::mach_port_deallocate;
    use mach2::mach_time::mach_absolute_time;
    use mach2::mach_types::{thread_act_array_t, thread_act_t};
    use mach2::message::mach_msg_type_number_t;
    use mach2::port::mach_port_t;
    use mach2::task::task_threads;
    use mach2::thread_act::{thread_get_state, thread_resume, thread_suspend};
    use mach2::thread_status::thread_state_t;
    use mach2::traps::mach_task_self;
    use mach2::vm::{mach_vm_deallocate, mach_vm_region};
    use mach2::vm_prot::VM_PROT_READ;
    use mach2::vm_region::{VM_REGION_BASIC_INFO_64, vm_region_basic_info_64};
    use mach2::vm_types::{mach_vm_address_t, mach_vm_size_t, natural_t};

    const HELPER_MAGIC: u32 = 0x3158_5453;
    const HELPER_OP_CAPTURE: u32 = 1;
    const HELPER_OP_HELLO: u32 = 2;
    const HELPER_STATUS_OK: u32 = 0;
    const HELPER_STATUS_ERR: u32 = 1;
    const REQUEST_BYTES: usize = 24;
    const RESPONSE_HEADER_BYTES: usize = 88;
    const STACK_SNAPSHOT_BYTES: usize = 512 * 1024;

    pub(super) struct StackHelper {
        threads: ThreadPortCache,
        helper_tid: u32,
    }

    impl StackHelper {
        pub(super) fn new() -> Self {
            Self {
                threads: ThreadPortCache::new(),
                helper_tid: current_thread_id().unwrap_or(0),
            }
        }

        pub(super) fn serve(&mut self, mut stream: UnixStream) {
            loop {
                let mut request = [0u8; REQUEST_BYTES];
                if let Err(err) = stream.read_exact(&mut request) {
                    if err.kind() != std::io::ErrorKind::UnexpectedEof {
                        eprintln!("stax-workload: helper read failed: {err}");
                    }
                    return;
                }
                let magic = read_u32(&request, 0);
                let op = read_u32(&request, 4);
                let seq = read_u64(&request, 8);
                let tid = read_u32(&request, 16);
                let write_result = if magic != HELPER_MAGIC {
                    write_response(&mut stream, seq, None)
                } else if op == HELPER_OP_HELLO {
                    write_hello_response(&mut stream, seq, self.helper_tid)
                } else if op == HELPER_OP_CAPTURE {
                    write_response(&mut stream, seq, self.capture(tid))
                } else {
                    write_response(&mut stream, seq, None)
                };
                if let Err(err) = write_result {
                    eprintln!("stax-workload: helper write failed: {err}");
                    return;
                }
            }
        }

        fn capture(&mut self, tid: u32) -> Option<Capture> {
            if tid == self.helper_tid {
                return None;
            }
            let thread = self.threads.get(tid)?;
            let thread_lookup_done = unsafe { mach_absolute_time() };
            let kr = unsafe { thread_suspend(thread) };
            if kr != KERN_SUCCESS {
                self.threads.forget(tid);
                return None;
            }

            let state = match read_thread_state(thread) {
                Some(state) => state,
                None => {
                    let _ = unsafe { thread_resume(thread) };
                    self.threads.forget(tid);
                    return None;
                }
            };
            let stack = copy_stack_window_live(state.sp);
            let state_done = unsafe { mach_absolute_time() };
            let resume_kr = unsafe { thread_resume(thread) };
            let resume_done = unsafe { mach_absolute_time() };
            if resume_kr != KERN_SUCCESS {
                self.threads.forget(tid);
                return None;
            }

            Some(Capture {
                thread_lookup_done,
                state_done,
                resume_done,
                pc: strip_code_ptr(state.pc),
                lr: strip_code_ptr(state.lr),
                fp: strip_data_ptr(state.fp),
                sp: strip_data_ptr(state.sp),
                stack,
            })
        }
    }

    struct Capture {
        thread_lookup_done: u64,
        state_done: u64,
        resume_done: u64,
        pc: u64,
        lr: u64,
        fp: u64,
        sp: u64,
        stack: StackSnapshot,
    }

    struct StackSnapshot {
        base: u64,
        bytes: Vec<u8>,
    }

    fn write_response(
        stream: &mut UnixStream,
        seq: u64,
        capture: Option<Capture>,
    ) -> std::io::Result<()> {
        let mut header = [0u8; RESPONSE_HEADER_BYTES];
        write_u32(&mut header, 0, HELPER_MAGIC);
        write_u32(
            &mut header,
            4,
            if capture.is_some() {
                HELPER_STATUS_OK
            } else {
                HELPER_STATUS_ERR
            },
        );
        write_u64(&mut header, 8, seq);
        if let Some(capture) = &capture {
            write_u64(&mut header, 16, capture.thread_lookup_done);
            write_u64(&mut header, 24, capture.state_done);
            write_u64(&mut header, 32, capture.resume_done);
            write_u64(&mut header, 40, capture.stack.base);
            write_u64(&mut header, 48, capture.pc);
            write_u64(&mut header, 56, capture.lr);
            write_u64(&mut header, 64, capture.fp);
            write_u64(&mut header, 72, capture.sp);
            write_u32(&mut header, 80, capture.stack.bytes.len() as u32);
        }
        stream.write_all(&header)?;
        if let Some(capture) = capture {
            stream.write_all(&capture.stack.bytes)?;
        }
        Ok(())
    }

    fn write_hello_response(
        stream: &mut UnixStream,
        seq: u64,
        helper_tid: u32,
    ) -> std::io::Result<()> {
        let mut header = [0u8; RESPONSE_HEADER_BYTES];
        write_u32(&mut header, 0, HELPER_MAGIC);
        write_u32(&mut header, 4, HELPER_STATUS_OK);
        write_u64(&mut header, 8, seq);
        write_u64(&mut header, 48, u64::from(helper_tid));
        stream.write_all(&header)
    }

    struct ThreadPortCache {
        by_tid: HashMap<u32, thread_act_t>,
    }

    impl ThreadPortCache {
        fn new() -> Self {
            Self {
                by_tid: HashMap::new(),
            }
        }

        fn get(&mut self, tid: u32) -> Option<thread_act_t> {
            if let Some(&thread) = self.by_tid.get(&tid) {
                return Some(thread);
            }
            self.refresh();
            self.by_tid.get(&tid).copied()
        }

        fn forget(&mut self, tid: u32) {
            if let Some(thread) = self.by_tid.remove(&tid) {
                deallocate_port(thread);
            }
        }

        fn refresh(&mut self) {
            let mut list: thread_act_array_t = std::ptr::null_mut();
            let mut count: mach_msg_type_number_t = 0;
            let kr = unsafe { task_threads(mach_task_self(), &mut list, &mut count) };
            if kr != KERN_SUCCESS {
                return;
            }
            let threads = unsafe { std::slice::from_raw_parts(list, count as usize) };
            for &thread in threads {
                match thread_id(thread) {
                    Some(tid) => {
                        if self.by_tid.contains_key(&tid) {
                            deallocate_port(thread);
                        } else {
                            self.by_tid.insert(tid, thread);
                        }
                    }
                    None => deallocate_port(thread),
                }
            }
            let bytes = count as u64 * std::mem::size_of::<thread_act_t>() as u64;
            let _ =
                unsafe { mach_vm_deallocate(mach_task_self(), list as mach_vm_address_t, bytes) };
        }
    }

    impl Drop for ThreadPortCache {
        fn drop(&mut self) {
            for (_, thread) in self.by_tid.drain() {
                deallocate_port(thread);
            }
        }
    }

    fn current_thread_id() -> Option<u32> {
        let mut tid = 0u64;
        let rc = unsafe { libc::pthread_threadid_np(0, &mut tid) };
        if rc == 0 {
            u32::try_from(tid).ok()
        } else {
            None
        }
    }

    fn thread_id(thread: thread_act_t) -> Option<u32> {
        let mut info = libc::thread_identifier_info_data_t {
            thread_id: 0,
            thread_handle: 0,
            dispatch_qaddr: 0,
        };
        let mut count = libc::THREAD_IDENTIFIER_INFO_COUNT;
        let kr = unsafe {
            libc::thread_info(
                thread,
                libc::THREAD_IDENTIFIER_INFO as u32,
                (&mut info as *mut libc::thread_identifier_info_data_t).cast(),
                &mut count,
            )
        };
        if kr == KERN_SUCCESS {
            u32::try_from(info.thread_id).ok()
        } else {
            None
        }
    }

    fn deallocate_port(port: mach_port_t) {
        let _ = unsafe { mach_port_deallocate(mach_task_self(), port) };
    }

    #[derive(Clone, Copy)]
    struct ThreadState {
        pc: u64,
        lr: u64,
        fp: u64,
        sp: u64,
    }

    #[cfg(target_arch = "aarch64")]
    fn read_thread_state(thread: thread_act_t) -> Option<ThreadState> {
        #[repr(C)]
        #[derive(Default)]
        struct ArmThreadState64 {
            x: [u64; 29],
            fp: u64,
            lr: u64,
            sp: u64,
            pc: u64,
            cpsr: u32,
            pad: u32,
        }

        let mut state = ArmThreadState64::default();
        let mut count: mach_msg_type_number_t =
            (std::mem::size_of::<ArmThreadState64>() / std::mem::size_of::<natural_t>()) as _;
        let kr = unsafe {
            thread_get_state(
                thread,
                mach2::thread_status::ARM_THREAD_STATE64,
                (&mut state as *mut ArmThreadState64).cast::<natural_t>() as thread_state_t,
                &mut count,
            )
        };
        if kr == KERN_SUCCESS {
            Some(ThreadState {
                pc: state.pc,
                lr: state.lr,
                fp: state.fp,
                sp: state.sp,
            })
        } else {
            None
        }
    }

    #[cfg(target_arch = "x86_64")]
    fn read_thread_state(thread: thread_act_t) -> Option<ThreadState> {
        #[repr(C)]
        #[derive(Default)]
        struct X86ThreadState64 {
            rax: u64,
            rbx: u64,
            rcx: u64,
            rdx: u64,
            rdi: u64,
            rsi: u64,
            rbp: u64,
            rsp: u64,
            r8: u64,
            r9: u64,
            r10: u64,
            r11: u64,
            r12: u64,
            r13: u64,
            r14: u64,
            r15: u64,
            rip: u64,
            rflags: u64,
            cs: u64,
            fs: u64,
            gs: u64,
        }

        let mut state = X86ThreadState64::default();
        let mut count: mach_msg_type_number_t =
            (std::mem::size_of::<X86ThreadState64>() / std::mem::size_of::<natural_t>()) as _;
        let kr = unsafe {
            thread_get_state(
                thread,
                mach2::thread_status::x86_THREAD_STATE64,
                (&mut state as *mut X86ThreadState64).cast::<natural_t>() as thread_state_t,
                &mut count,
            )
        };
        if kr == KERN_SUCCESS {
            Some(ThreadState {
                pc: state.rip,
                lr: 0,
                fp: state.rbp,
                sp: state.rsp,
            })
        } else {
            None
        }
    }

    fn copy_stack_window_live(sp: u64) -> StackSnapshot {
        let base = strip_data_ptr(sp);
        let max_len = readable_region_remaining(base)
            .unwrap_or(0)
            .min(STACK_SNAPSHOT_BYTES as u64) as usize;
        let mut bytes = vec![0u8; max_len];
        if max_len != 0 {
            unsafe {
                std::ptr::copy_nonoverlapping(base as *const u8, bytes.as_mut_ptr(), max_len);
            }
        }
        StackSnapshot { base, bytes }
    }

    fn readable_region_remaining(address: u64) -> Option<u64> {
        let mut region_address = address as mach_vm_address_t;
        let mut region_size: mach_vm_size_t = 0;
        let mut info = vm_region_basic_info_64::default();
        let mut count = vm_region_basic_info_64::count();
        let mut object_name: mach_port_t = 0;
        let kr = unsafe {
            mach_vm_region(
                mach_task_self(),
                &mut region_address,
                &mut region_size,
                VM_REGION_BASIC_INFO_64,
                (&mut info as *mut vm_region_basic_info_64).cast(),
                &mut count,
                &mut object_name,
            )
        };
        if object_name != 0 {
            deallocate_port(object_name);
        }
        if kr != KERN_SUCCESS || region_address > address || info.protection & VM_PROT_READ == 0 {
            return None;
        }
        Some(
            region_address
                .saturating_add(region_size)
                .saturating_sub(address),
        )
    }

    #[cfg(target_arch = "aarch64")]
    fn strip_code_ptr(mut ptr: u64) -> u64 {
        unsafe {
            core::arch::asm!(
                "xpaci {ptr}",
                ptr = inout(reg) ptr,
                options(nomem, nostack, preserves_flags)
            );
        }
        ptr
    }

    #[cfg(target_arch = "aarch64")]
    fn strip_data_ptr(mut ptr: u64) -> u64 {
        unsafe {
            core::arch::asm!(
                "xpacd {ptr}",
                ptr = inout(reg) ptr,
                options(nomem, nostack, preserves_flags)
            );
        }
        ptr
    }

    #[cfg(not(target_arch = "aarch64"))]
    fn strip_code_ptr(ptr: u64) -> u64 {
        ptr
    }

    #[cfg(not(target_arch = "aarch64"))]
    fn strip_data_ptr(ptr: u64) -> u64 {
        ptr
    }

    fn read_u32(buf: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(buf[offset..offset + 4].try_into().expect("u32 field"))
    }

    fn read_u64(buf: &[u8], offset: usize) -> u64 {
        u64::from_le_bytes(buf[offset..offset + 8].try_into().expect("u64 field"))
    }

    fn write_u32(buf: &mut [u8], offset: usize, value: u32) {
        buf[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn write_u64(buf: &mut [u8], offset: usize, value: u64) {
        buf[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }
}

#[cfg(target_os = "macos")]
use mac_helper::StackHelper;
