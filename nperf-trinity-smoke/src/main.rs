//! Trinity smoke test.
//!
//! Connects to both legs of the trinity and prints what came back, so
//! we can validate the architecture works end-to-end before wiring it
//! into nperf-live proper.
//!
//! Prerequisites: both `nperfd` and `nperf-task-broker` are running
//! (typically via launchd; see `cargo xtask build-daemon` /
//! `cargo xtask build-broker` for install instructions).
//!
//! Usage:
//!
//!     # default daemon socket: /tmp/nperfd.sock
//!     cargo run -p nperf-trinity-smoke
//!
//!     # custom daemon socket (e.g. when running under launchd):
//!     cargo run -p nperf-trinity-smoke -- /var/run/nperfd.sock

#![cfg(target_os = "macos")]

use std::ffi::CString;
use std::mem::{self, MaybeUninit};

use mach2::bootstrap::{BOOTSTRAP_SUCCESS, bootstrap_look_up, bootstrap_port};
use mach2::kern_return::KERN_SUCCESS;
use mach2::mach_port::mach_port_allocate;
use mach2::message::{
    MACH_MSGH_BITS, MACH_MSG_SUCCESS, MACH_MSG_TIMEOUT_NONE, MACH_MSG_TYPE_COPY_SEND,
    MACH_MSG_TYPE_MAKE_SEND_ONCE, MACH_RCV_MSG, MACH_SEND_MSG, mach_msg, mach_msg_body_t,
    mach_msg_header_t, mach_msg_port_descriptor_t, mach_msg_size_t,
};
use mach2::port::{MACH_PORT_NULL, MACH_PORT_RIGHT_RECEIVE, mach_port_t};
use mach2::traps::mach_task_self;
use nperfd_proto::NperfdClient;

const DAEMON_DEFAULT_SOCKET: &str = "/tmp/nperfd.sock";
const BROKER_SERVICE: &str = "eu.bearcove.nperf.task-broker";

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let socket_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| DAEMON_DEFAULT_SOCKET.into());

    println!(":: trinity smoke test");
    println!();

    println!(":: 1/2 — nperfd (root daemon, kperf+kdebug owner)");
    let url = format!("local://{socket_path}");
    println!("       connecting to {url}");
    let client: NperfdClient = vox::connect(&url).await?;
    let status = client.status().await?;
    println!("       version : {}", status.version);
    println!("       arch    : {}", status.host_arch);
    println!("       state   : {:?}", status.state);
    println!();

    println!(":: 2/2 — nperf-task-broker (same-uid Mach IPC, cs.debugger)");
    let self_pid = std::process::id() as i32;
    println!("       asking broker for task port of pid={self_pid}");
    let task_port = ask_broker_for_task(self_pid)?;
    println!("       task_port : {task_port:#x}");
    println!();

    println!(":: trinity OK — both legs alive, ready to wire into the recorder.");
    Ok(())
}

/// One round-trip with the broker: bootstrap_look_up, allocate a
/// reply port, build + send a request, receive the reply.
fn ask_broker_for_task(pid: i32) -> Result<mach_port_t, Box<dyn std::error::Error>> {
    let service_cstr = CString::new(BROKER_SERVICE)?;
    let mut broker_port: mach_port_t = MACH_PORT_NULL;
    let kr = unsafe {
        bootstrap_look_up(bootstrap_port, service_cstr.as_ptr(), &mut broker_port)
    };
    if kr != BOOTSTRAP_SUCCESS as i32 {
        return Err(format!(
            "bootstrap_look_up({BROKER_SERVICE}) failed: {kr:#x} \
             (LaunchAgent installed and loaded?)"
        )
        .into());
    }

    let task_self = unsafe { mach_task_self() };
    let mut reply_port: mach_port_t = MACH_PORT_NULL;
    let kr = unsafe {
        mach_port_allocate(task_self, MACH_PORT_RIGHT_RECEIVE, &mut reply_port)
    };
    if kr != KERN_SUCCESS {
        return Err(format!("mach_port_allocate(reply): {kr:#x}").into());
    }

    // Wire format: must match `nperf-task-broker/src/main.rs` exactly.
    #[repr(C)]
    struct RequestMsg {
        header: mach_msg_header_t,
        pid: i32,
        _pad: u32,
    }
    /// Reply layout including space for the trailer the kernel
    /// always appends to received messages (a `mach_msg_trailer_t` =
    /// 8 bytes; we reserve a bit more to be safe).
    #[repr(C)]
    struct ReplyMsg {
        header: mach_msg_header_t,
        body: mach_msg_body_t,
        task_port: mach_msg_port_descriptor_t,
        error: i32,
        _pad: u32,
        _trailer_space: [u8; 32],
    }

    let mut req = RequestMsg {
        header: mach_msg_header_t {
            msgh_bits: MACH_MSGH_BITS(
                MACH_MSG_TYPE_COPY_SEND,
                MACH_MSG_TYPE_MAKE_SEND_ONCE,
            ),
            msgh_size: mem::size_of::<RequestMsg>() as mach_msg_size_t,
            msgh_remote_port: broker_port,
            msgh_local_port: reply_port,
            msgh_voucher_port: 0,
            msgh_id: 1,
        },
        pid,
        _pad: 0,
    };

    let kr = unsafe {
        mach_msg(
            &mut req.header as *mut _,
            MACH_SEND_MSG,
            mem::size_of::<RequestMsg>() as mach_msg_size_t,
            0,
            MACH_PORT_NULL,
            MACH_MSG_TIMEOUT_NONE,
            MACH_PORT_NULL,
        )
    };
    if kr != MACH_MSG_SUCCESS {
        return Err(format!("mach_msg send to broker: {kr:#x}").into());
    }

    let mut reply: MaybeUninit<ReplyMsg> = MaybeUninit::uninit();
    let kr = unsafe {
        mach_msg(
            reply.as_mut_ptr() as *mut mach_msg_header_t,
            MACH_RCV_MSG,
            0,
            mem::size_of::<ReplyMsg>() as mach_msg_size_t,
            reply_port,
            MACH_MSG_TIMEOUT_NONE,
            MACH_PORT_NULL,
        )
    };
    if kr != MACH_MSG_SUCCESS {
        return Err(format!("mach_msg recv reply: {kr:#x}").into());
    }
    let reply = unsafe { reply.assume_init() };

    if reply.error != KERN_SUCCESS {
        return Err(format!(
            "broker task_for_pid returned error: {:#x} (kperf-broker insufficient?)",
            reply.error
        )
        .into());
    }

    Ok(reply.task_port.name)
}
