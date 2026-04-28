//! End-to-end check: connect to staxd, ask it to record a target
//! pid for a few seconds, count the samples + image / thread events
//! that come back. Lets us validate the full daemon → client → parser
//! → SampleSink path without touching stax-live.
//!
//! Usage:
//!
//!     # in one terminal: start the daemon (root)
//!     sudo ./target/release/staxd --socket /tmp/staxd-test.sock
//!
//!     # in another terminal: run the demo against a same-uid pid
//!     cargo run --example record_pid -p staxd-client -- \
//!         --pid <PID> --duration 5 --socket /tmp/staxd-test.sock

#[cfg(target_os = "macos")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::time::Duration;

    use stax_mac_capture::{
        BinaryLoadedEvent, BinaryUnloadedEvent, SampleEvent, SampleSink, ThreadNameEvent,
    };
    use staxd_client::{RemoteOptions, drive_session};

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "staxd_client=info,record_pid=info".into()),
        )
        .init();

    let mut pid: Option<u32> = None;
    let mut duration_secs: u64 = 5;
    let mut socket = "/tmp/staxd.sock".to_string();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--pid" => {
                pid = Some(args.next().ok_or("--pid takes a value")?.parse()?);
            }
            "--duration" => {
                duration_secs = args.next().ok_or("--duration takes a value")?.parse()?;
            }
            "--socket" => {
                socket = args.next().ok_or("--socket takes a value")?;
            }
            other => return Err(format!("unknown arg: {other}").into()),
        }
    }
    let pid = pid.ok_or("--pid is required")?;

    struct CountingSink {
        samples: u64,
        binaries_loaded: u64,
        thread_names: u64,
        wakeups: u64,
        first_few_samples: u32,
    }
    impl SampleSink for CountingSink {
        fn on_sample(&mut self, ev: SampleEvent<'_>) {
            self.samples += 1;
            if self.first_few_samples < 3
                && (!ev.backtrace.is_empty() || !ev.kernel_backtrace.is_empty())
            {
                self.first_few_samples += 1;
                println!(
                    "[sample] tid={} ts={} u={} k={} cycles={} insns={}",
                    ev.tid,
                    ev.timestamp_ns,
                    ev.backtrace.len(),
                    ev.kernel_backtrace.len(),
                    ev.cycles,
                    ev.instructions,
                );
            }
        }
        fn on_binary_loaded(&mut self, ev: BinaryLoadedEvent<'_>) {
            self.binaries_loaded += 1;
            if self.binaries_loaded <= 3 {
                println!(
                    "[binary] {} ({} bytes, {} symbols)",
                    ev.path,
                    ev.vmsize,
                    ev.symbols.len()
                );
            }
        }
        fn on_binary_unloaded(&mut self, _: BinaryUnloadedEvent<'_>) {}
        fn on_thread_name(&mut self, ev: ThreadNameEvent<'_>) {
            self.thread_names += 1;
            if self.thread_names <= 3 {
                println!("[thread] tid={} name={}", ev.tid, ev.name);
            }
        }
        fn on_wakeup(&mut self, _: stax_mac_capture::WakeupEvent<'_>) {
            self.wakeups += 1;
        }
    }

    let opts = RemoteOptions {
        daemon_socket: socket,
        pid,
        frequency_hz: 1000,
        duration: Some(Duration::from_secs(duration_secs)),
        ..Default::default()
    };

    let mut sink = CountingSink {
        samples: 0,
        binaries_loaded: 0,
        thread_names: 0,
        wakeups: 0,
        first_few_samples: 0,
    };

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(drive_session(opts, &mut sink, || false))?;

    println!();
    println!(":: record_pid done");
    println!("   samples           : {}", sink.samples);
    println!("   binaries_loaded   : {}", sink.binaries_loaded);
    println!("   thread_names      : {}", sink.thread_names);
    println!("   wakeups           : {}", sink.wakeups);
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("staxd-client is macOS-only");
}
