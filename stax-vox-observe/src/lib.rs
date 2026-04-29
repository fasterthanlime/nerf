use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use stax_telemetry::{
    CounterHandle, GaugeHandle, HistogramHandle, HistogramSnapshot, TelemetryRegistry,
    TelemetrySnapshot,
};

const SLOW_CHANNEL_SEND: Duration = Duration::from_millis(10);
const SLOW_REQUEST: Duration = Duration::from_millis(10);

static GLOBAL_DEBUG_REGISTRY: OnceLock<VoxDebugRegistry> = OnceLock::new();
static GLOBAL_TELEMETRY_REGISTRY: OnceLock<TelemetryDebugRegistry> = OnceLock::new();

#[derive(Clone)]
pub struct VoxDebugRegistry {
    inner: Arc<VoxDebugRegistryInner>,
}

struct VoxDebugRegistryInner {
    next_id: AtomicU64,
    entries: Mutex<Vec<VoxDebugEntry>>,
}

#[derive(Clone)]
pub struct TelemetryDebugRegistry {
    inner: Arc<TelemetryDebugRegistryInner>,
}

struct TelemetryDebugRegistryInner {
    next_id: AtomicU64,
    entries: Mutex<Vec<TelemetryDebugEntry>>,
}

#[derive(Clone)]
struct VoxDebugEntry {
    id: u64,
    component: &'static str,
    surface: &'static str,
    role: &'static str,
    caller: vox::Caller,
}

#[derive(Clone)]
struct TelemetryDebugEntry {
    id: u64,
    component: &'static str,
    role: &'static str,
    registry: TelemetryRegistry,
}

pub struct VoxDebugRegistration {
    id: u64,
    inner: Weak<VoxDebugRegistryInner>,
}

pub struct TelemetryDebugRegistration {
    id: u64,
    inner: Weak<TelemetryDebugRegistryInner>,
}

#[derive(Clone)]
pub struct VoxObserverLogger {
    component: &'static str,
    surface: &'static str,
    pid: Option<u32>,
    telemetry: Option<VoxObserverTelemetry>,
}

#[derive(Clone)]
struct VoxObserverTelemetry {
    registry: TelemetryRegistry,
    rpc_started: CounterHandle,
    rpc_finished: CounterHandle,
    rpc_failed: CounterHandle,
    rpc_elapsed: HistogramHandle,
    connections_opened: CounterHandle,
    connections_closed: CounterHandle,
    active_connections: GaugeHandle,
    driver_requests_started: CounterHandle,
    driver_requests_finished: CounterHandle,
    driver_requests_failed: CounterHandle,
    driver_request_elapsed: HistogramHandle,
    outbound_queue_full: CounterHandle,
    outbound_queue_closed: CounterHandle,
    driver_frame_read_bytes: CounterHandle,
    driver_frame_written_bytes: CounterHandle,
    driver_errors: CounterHandle,
    channel_opened: CounterHandle,
    channel_closed: CounterHandle,
    channel_reset: CounterHandle,
    channel_send_started: CounterHandle,
    channel_send_waiting_for_credit: CounterHandle,
    channel_send_finished: CounterHandle,
    channel_send_failed: CounterHandle,
    channel_send_elapsed: HistogramHandle,
    channel_try_send: CounterHandle,
    channel_try_send_failed: CounterHandle,
    channel_credit_grants: CounterHandle,
    channel_credit_granted: CounterHandle,
    channel_item_received: CounterHandle,
    channel_item_consumed: CounterHandle,
    transport_frame_read_bytes: CounterHandle,
    transport_frame_written_bytes: CounterHandle,
    transport_closed: CounterHandle,
}

pub fn global_debug_registry() -> &'static VoxDebugRegistry {
    GLOBAL_DEBUG_REGISTRY.get_or_init(VoxDebugRegistry::new)
}

pub fn global_telemetry_registry() -> &'static TelemetryDebugRegistry {
    GLOBAL_TELEMETRY_REGISTRY.get_or_init(TelemetryDebugRegistry::new)
}

pub fn register_global_caller(
    component: &'static str,
    surface: &'static str,
    role: &'static str,
    caller: &vox::Caller,
) -> VoxDebugRegistration {
    global_debug_registry().register_caller(component, surface, role, caller)
}

pub fn register_global_telemetry(
    component: &'static str,
    role: &'static str,
    registry: TelemetryRegistry,
) -> TelemetryDebugRegistration {
    global_telemetry_registry().register(component, role, registry)
}

#[must_use]
pub fn install_global_sigusr1_dump(
    process_name: &'static str,
) -> Option<tokio::task::JoinHandle<()>> {
    global_debug_registry().install_sigusr1_dump(process_name)
}

impl VoxDebugRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(VoxDebugRegistryInner {
                next_id: AtomicU64::new(1),
                entries: Mutex::new(Vec::new()),
            }),
        }
    }

    pub fn register_caller(
        &self,
        component: &'static str,
        surface: &'static str,
        role: &'static str,
        caller: &vox::Caller,
    ) -> VoxDebugRegistration {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        self.inner
            .entries
            .lock()
            .expect("vox debug registry mutex poisoned")
            .push(VoxDebugEntry {
                id,
                component,
                surface,
                role,
                caller: caller.clone(),
            });
        VoxDebugRegistration {
            id,
            inner: Arc::downgrade(&self.inner),
        }
    }

    pub fn dump_debug_snapshots(&self, process_name: &'static str, reason: &'static str) {
        let entries = self
            .inner
            .entries
            .lock()
            .expect("vox debug registry mutex poisoned")
            .clone();
        tracing::info!(
            process = process_name,
            reason,
            handles = entries.len(),
            "dumping registered vox debug snapshots"
        );
        if entries.is_empty() {
            return;
        }
        for entry in entries {
            let snapshot = entry.caller.debug_snapshot();
            let formatted = format_debug_snapshot(&snapshot);
            tracing::info!(
                process = process_name,
                reason,
                component = entry.component,
                surface = entry.surface,
                role = entry.role,
                registration_id = entry.id,
                "\n{formatted}"
            );
        }
    }

    pub fn dump_all(&self, process_name: &'static str, reason: &'static str) {
        global_telemetry_registry().dump_telemetry_snapshots(process_name, reason);
        self.dump_debug_snapshots(process_name, reason);
    }

    #[must_use]
    pub fn install_sigusr1_dump(
        &self,
        process_name: &'static str,
    ) -> Option<tokio::task::JoinHandle<()>> {
        #[cfg(unix)]
        {
            let registry = self.clone();
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                return match tokio::signal::unix::signal(
                    tokio::signal::unix::SignalKind::user_defined1(),
                ) {
                    Ok(mut signal) => Some(handle.spawn(async move {
                        while signal.recv().await.is_some() {
                            registry.dump_all(process_name, "SIGUSR1");
                        }
                    })),
                    Err(error) => {
                        tracing::warn!(
                            process = process_name,
                            ?error,
                            "failed to install SIGUSR1 vox debug dump handler"
                        );
                        None
                    }
                };
            }

            let _ = std::thread::Builder::new()
                .name(format!("{process_name}-vox-sigusr1"))
                .spawn(move || {
                    let runtime = match tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        Ok(runtime) => runtime,
                        Err(error) => {
                            tracing::warn!(
                                process = process_name,
                                ?error,
                                "failed to create SIGUSR1 vox debug dump runtime"
                            );
                            return;
                        }
                    };
                    runtime.block_on(async move {
                        let mut signal = match tokio::signal::unix::signal(
                            tokio::signal::unix::SignalKind::user_defined1(),
                        ) {
                            Ok(signal) => signal,
                            Err(error) => {
                                tracing::warn!(
                                    process = process_name,
                                    ?error,
                                    "failed to install SIGUSR1 vox debug dump handler"
                                );
                                return;
                            }
                        };
                        while signal.recv().await.is_some() {
                            registry.dump_all(process_name, "SIGUSR1");
                        }
                    });
                });
            None
        }
        #[cfg(not(unix))]
        {
            let _ = process_name;
            None
        }
    }
}

impl TelemetryDebugRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(TelemetryDebugRegistryInner {
                next_id: AtomicU64::new(1),
                entries: Mutex::new(Vec::new()),
            }),
        }
    }

    pub fn register(
        &self,
        component: &'static str,
        role: &'static str,
        registry: TelemetryRegistry,
    ) -> TelemetryDebugRegistration {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        self.inner
            .entries
            .lock()
            .expect("telemetry debug registry mutex poisoned")
            .push(TelemetryDebugEntry {
                id,
                component,
                role,
                registry,
            });
        TelemetryDebugRegistration {
            id,
            inner: Arc::downgrade(&self.inner),
        }
    }

    pub fn dump_telemetry_snapshots(&self, process_name: &'static str, reason: &'static str) {
        let entries = self
            .inner
            .entries
            .lock()
            .expect("telemetry debug registry mutex poisoned")
            .clone();
        tracing::info!(
            process = process_name,
            reason,
            handles = entries.len(),
            "dumping registered telemetry snapshots"
        );
        for entry in entries {
            let snapshot = entry.registry.snapshot();
            let formatted = format_telemetry_snapshot(&snapshot);
            tracing::info!(
                process = process_name,
                reason,
                component = entry.component,
                role = entry.role,
                registration_id = entry.id,
                "\n{formatted}"
            );
        }
    }
}

fn format_debug_snapshot(snapshot: &vox::VoxDebugSnapshot) -> String {
    let now = Instant::now();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# Vox Debug Snapshot\n\n- connections: {}",
        snapshot.connections.len()
    );

    for connection in &snapshot.connections {
        let _ = writeln!(
            out,
            "\n## Connection {:?}\n\n- state: {:?}\n- driver: {:?}",
            connection.connection_id, connection.state, connection.driver_task_status
        );
        let _ = writeln!(
            out,
            "- endpoint: {}\n- surface: {}\n- component: {}\n- close_reason: {}",
            display_opt_debug(&connection.endpoint),
            display_opt_debug(&connection.surface),
            display_opt_debug(&connection.component),
            display_opt_debug(&connection.close_reason)
        );
        let _ = writeln!(
            out,
            "- queues: outbound={}/{} · local_control={}/{}",
            display_opt_usize(connection.outbound_queue_depth),
            display_opt_usize(connection.outbound_queue_capacity),
            display_opt_usize(connection.local_control_queue_depth),
            display_opt_usize(connection.local_control_queue_capacity)
        );
        let _ = writeln!(
            out,
            "- last: inbound={} · outbound={} · progress={}",
            instant_age(connection.last_inbound_message_at, now),
            instant_age(connection.last_outbound_message_at, now),
            instant_age(connection.last_progress_at, now)
        );

        if !connection.requests.is_empty() {
            let _ = writeln!(
                out,
                "\n### Requests\n\n- outstanding: {}\n- tracked: {}",
                connection.outstanding_requests,
                connection.requests.len(),
            );
            for request in &connection.requests {
                let _ = writeln!(
                    out,
                    "\n#### {:?}\n\n- state: {:?}\n- age: {}\n- method: {}::{}\n- method_id: {:?}\n- response_sender_blocked: {}\n- associated_channels: {}",
                    request.request_id,
                    request.state,
                    format_duration(request.age),
                    request.service.unwrap_or("?"),
                    request.method.unwrap_or("?"),
                    request.method_id,
                    display_opt_bool(request.response_sender_blocked),
                    format_channel_ids(&request.associated_channels)
                );
            }
        }

        if !connection.open_channels.is_empty() {
            let _ = writeln!(
                out,
                "\n### Channels\n\n- open: {}",
                connection.open_channels.len()
            );
            for channel in &connection.open_channels {
                let _ = writeln!(
                    out,
                    "\n#### {:?}/{:?} · {:?}\n\n{}",
                    channel.connection_id,
                    channel.channel_id,
                    channel.direction,
                    format_channel_debug_block(channel.debug)
                );
                let _ = writeln!(
                    out,
                    "- credit:\n  - initial: {}\n  - available_send_credit: {}\n  - current_permit_count: {}\n  - pending_local_grant_credit: {}\n  - total_credit_granted: {}\n  - total_credit_received: {}\n  - last_credit_granted: {}\n  - last_credit_received: {}",
                    channel.initial_credit,
                    display_opt_u32(channel.available_send_credit),
                    display_opt_u32(channel.current_permit_count),
                    channel.pending_local_grant_credit,
                    channel.total_credit_granted,
                    channel.total_credit_received,
                    instant_age_with_amount(
                        channel.last_credit_granted_at,
                        channel.last_credit_granted_amount,
                        now
                    ),
                    instant_age_with_amount(
                        channel.last_credit_received_at,
                        channel.last_credit_received_amount,
                        now
                    )
                );
                let _ = writeln!(
                    out,
                    "- receive:\n  - state: {:?}\n  - queue: {}/{}\n  - items_received: {}\n  - items_consumed: {}\n  - last_item_received: {}\n  - last_item_consumed: {}",
                    channel.receiver_state,
                    display_opt_usize(channel.inbound_queue_len),
                    display_opt_usize(channel.inbound_queue_capacity),
                    channel.items_received,
                    channel.items_consumed,
                    instant_age(channel.last_item_received_at, now),
                    instant_age(channel.last_item_consumed_at, now)
                );
                let _ = writeln!(
                    out,
                    "- send:\n  - sent: {}\n  - sends_started: {}\n  - sends_completed: {}\n  - sends_waited_for_credit: {}\n  - send_waiters_count: {}\n  - zero_credit_with_blocked_senders: {}\n  - outbound_runtime_queue: {}/{}\n  - last_item_sent: {}",
                    channel.sent,
                    channel.sends_started,
                    channel.sends_completed,
                    channel.sends_waited_for_credit,
                    display_opt_usize(channel.send_waiters_count),
                    channel.zero_credit_with_blocked_senders,
                    display_opt_usize(channel.outbound_runtime_queue_len),
                    display_opt_usize(channel.outbound_runtime_queue_capacity),
                    instant_age(channel.last_item_sent_at, now)
                );
                let _ = writeln!(
                    out,
                    "- failures:\n  - try_send_full_credit: {}\n  - try_send_full_runtime_queue: {}\n  - closed: {}\n  - reset: {}\n  - dropped: {}\n  - close_reason: {}\n  - reset_reason: {}",
                    channel.try_send_full_credit,
                    channel.try_send_full_runtime_queue,
                    channel.closed,
                    channel.reset,
                    channel.dropped,
                    display_opt_debug(&channel.close_reason),
                    display_opt_debug(&channel.reset_reason)
                );
            }
        }
    }

    out
}

fn format_channel_debug_block(debug: Option<vox::ChannelDebugContext>) -> String {
    let Some(debug) = debug else {
        return "- debug: none".to_owned();
    };
    format!(
        "- debug:\n  - type: {}\n  - label: {}\n  - service: {}\n  - method: {}\n  - source: {}",
        display_opt_str(debug.type_name),
        display_opt_str(debug.label),
        display_opt_str(debug.service),
        display_opt_str(debug.method),
        format_source_location(debug.source_location)
    )
}

fn format_channel_debug_inline(debug: Option<vox::ChannelDebugContext>) -> String {
    let Some(debug) = debug else {
        return "debug=none".to_owned();
    };
    format!(
        "type={} label={} service={} method={} source={}",
        display_opt_str(debug.type_name),
        display_opt_str(debug.label),
        display_opt_str(debug.service),
        display_opt_str(debug.method),
        format_source_location(debug.source_location)
    )
}

fn display_opt_str(value: Option<&'static str>) -> &'static str {
    value.unwrap_or("-")
}

fn format_source_location(location: Option<vox::SourceLocation>) -> String {
    location
        .map(|loc| format!("{}:{}:{}", loc.file, loc.line, loc.column))
        .unwrap_or_else(|| "-".to_owned())
}

fn format_channel_ids(ids: &[vox::ChannelId]) -> String {
    if ids.is_empty() {
        return "[]".to_owned();
    }
    let mut out = String::from("[");
    for (index, id) in ids.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        let _ = write!(out, "{id:?}");
    }
    out.push(']');
    out
}

fn format_telemetry_snapshot(snapshot: &TelemetrySnapshot) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# Telemetry Snapshot\n\n- component: {}\n- generated_at_unix_ns: {}",
        snapshot.component, snapshot.generated_at_unix_ns
    );

    if !snapshot.phases.is_empty() {
        let _ = writeln!(out, "\n## Phases");
        for phase in &snapshot.phases {
            let _ = writeln!(
                out,
                "\n### `{}`\n\n- state: {}\n- elapsed: {}\n- entered_at_unix_ns: {}\n- detail: {}",
                phase.name,
                phase.state,
                format_duration(Duration::from_nanos(phase.elapsed_ns)),
                phase.entered_at_unix_ns,
                empty_as_dash(&phase.detail)
            );
        }
    }

    if !snapshot.gauges.is_empty() {
        let _ = writeln!(out, "\n## Gauges");
        for gauge in &snapshot.gauges {
            let _ = writeln!(out, "- `{}`: {}", gauge.name, gauge.value);
        }
    }

    if !snapshot.counters.is_empty() {
        let _ = writeln!(out, "\n## Counters");
        for counter in &snapshot.counters {
            let _ = writeln!(out, "- `{}`: {}", counter.name, counter.value);
        }
    }

    if !snapshot.histograms.is_empty() {
        let _ = writeln!(out, "\n## Histograms");
        for histogram in &snapshot.histograms {
            format_histogram(&mut out, histogram);
        }
    }

    if !snapshot.recent_events.is_empty() {
        let _ = writeln!(out, "\n## Recent Events");
        for event in &snapshot.recent_events {
            let _ = writeln!(
                out,
                "- `{}` `{}` {}",
                event.at_unix_ns,
                event.name,
                empty_as_dash(&event.detail)
            );
        }
    }

    out
}

fn format_histogram(out: &mut String, histogram: &HistogramSnapshot) {
    let avg = if histogram.count == 0 {
        0
    } else {
        histogram.sum / histogram.count
    };
    let _ = writeln!(
        out,
        "\n### `{}`\n\n- count: {}\n- avg: {}\n- max: {}\n- overflow: {}",
        histogram.name,
        histogram.count,
        format_duration(Duration::from_nanos(avg)),
        format_duration(Duration::from_nanos(histogram.max)),
        histogram.overflow
    );
    if histogram.buckets.iter().any(|bucket| bucket.count != 0) {
        let _ = writeln!(out, "- buckets:");
        for bucket in &histogram.buckets {
            if bucket.count != 0 {
                let _ = writeln!(
                    out,
                    "  - <= {}: {}",
                    format_duration(Duration::from_nanos(bucket.le)),
                    bucket.count
                );
            }
        }
    }
}

fn empty_as_dash(value: &str) -> &str {
    if value.is_empty() { "-" } else { value }
}

fn instant_age(instant: Option<Instant>, now: Instant) -> String {
    instant
        .map(|instant| {
            format!(
                "{} ago",
                format_duration(now.saturating_duration_since(instant))
            )
        })
        .unwrap_or_else(|| "-".to_owned())
}

fn instant_age_with_amount(instant: Option<Instant>, amount: Option<u32>, now: Instant) -> String {
    match (instant, amount) {
        (Some(instant), Some(amount)) => format!(
            "{} ago (+{})",
            format_duration(now.saturating_duration_since(instant)),
            amount
        ),
        (Some(instant), None) => format!(
            "{} ago",
            format_duration(now.saturating_duration_since(instant))
        ),
        (None, Some(amount)) => format!("never (+{})", amount),
        (None, None) => "-".to_owned(),
    }
}

fn format_duration(duration: Duration) -> String {
    let ns = duration.as_nanos();
    if ns >= 1_000_000_000 {
        format!("{:.3}s", ns as f64 / 1_000_000_000.0)
    } else if ns >= 1_000_000 {
        format!("{:.3}ms", ns as f64 / 1_000_000.0)
    } else if ns >= 1_000 {
        format!("{:.3}µs", ns as f64 / 1_000.0)
    } else {
        format!("{ns}ns")
    }
}

fn display_opt_usize(value: Option<usize>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "-".to_owned())
}

fn display_opt_u32(value: Option<u32>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "-".to_owned())
}

fn display_opt_bool(value: Option<bool>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "-".to_owned())
}

fn display_opt_debug<T: std::fmt::Debug>(value: &Option<T>) -> String {
    value
        .as_ref()
        .map(|value| format!("{value:?}"))
        .unwrap_or_else(|| "-".to_owned())
}

impl Default for VoxDebugRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl Default for TelemetryDebugRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for VoxDebugRegistration {
    fn drop(&mut self) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        inner
            .entries
            .lock()
            .expect("vox debug registry mutex poisoned")
            .retain(|entry| entry.id != self.id);
    }
}

impl Drop for TelemetryDebugRegistration {
    fn drop(&mut self) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        inner
            .entries
            .lock()
            .expect("telemetry debug registry mutex poisoned")
            .retain(|entry| entry.id != self.id);
    }
}

impl VoxObserverLogger {
    pub const fn new(component: &'static str, surface: &'static str) -> Self {
        Self {
            component,
            surface,
            pid: None,
            telemetry: None,
        }
    }

    pub const fn with_pid(mut self, pid: u32) -> Self {
        self.pid = Some(pid);
        self
    }

    pub fn with_telemetry(mut self, registry: TelemetryRegistry) -> Self {
        self.telemetry = Some(VoxObserverTelemetry::new(registry));
        self
    }
}

impl VoxObserverTelemetry {
    fn new(registry: TelemetryRegistry) -> Self {
        Self {
            rpc_started: registry.counter("vox.rpc.started"),
            rpc_finished: registry.counter("vox.rpc.finished"),
            rpc_failed: registry.counter("vox.rpc.failed"),
            rpc_elapsed: registry.histogram("vox.rpc.elapsed_ns"),
            connections_opened: registry.counter("vox.connection.opened"),
            connections_closed: registry.counter("vox.connection.closed"),
            active_connections: registry.gauge("vox.connection.active"),
            driver_requests_started: registry.counter("vox.driver.request.started"),
            driver_requests_finished: registry.counter("vox.driver.request.finished"),
            driver_requests_failed: registry.counter("vox.driver.request.failed"),
            driver_request_elapsed: registry.histogram("vox.driver.request.elapsed_ns"),
            outbound_queue_full: registry.counter("vox.driver.outbound_queue.full"),
            outbound_queue_closed: registry.counter("vox.driver.outbound_queue.closed"),
            driver_frame_read_bytes: registry.counter("vox.driver.frame_read.bytes"),
            driver_frame_written_bytes: registry.counter("vox.driver.frame_written.bytes"),
            driver_errors: registry.counter("vox.driver.errors"),
            channel_opened: registry.counter("vox.channel.opened"),
            channel_closed: registry.counter("vox.channel.closed"),
            channel_reset: registry.counter("vox.channel.reset"),
            channel_send_started: registry.counter("vox.channel.send.started"),
            channel_send_waiting_for_credit: registry
                .counter("vox.channel.send.waiting_for_credit"),
            channel_send_finished: registry.counter("vox.channel.send.finished"),
            channel_send_failed: registry.counter("vox.channel.send.failed"),
            channel_send_elapsed: registry.histogram("vox.channel.send.elapsed_ns"),
            channel_try_send: registry.counter("vox.channel.try_send"),
            channel_try_send_failed: registry.counter("vox.channel.try_send.failed"),
            channel_credit_grants: registry.counter("vox.channel.credit_grants"),
            channel_credit_granted: registry.counter("vox.channel.credit_granted"),
            channel_item_received: registry.counter("vox.channel.item_received"),
            channel_item_consumed: registry.counter("vox.channel.item_consumed"),
            transport_frame_read_bytes: registry.counter("vox.transport.frame_read.bytes"),
            transport_frame_written_bytes: registry.counter("vox.transport.frame_written.bytes"),
            transport_closed: registry.counter("vox.transport.closed"),
            registry,
        }
    }

    fn rpc_event(&self, surface: &'static str, event: vox::RpcEvent) {
        match event {
            vox::RpcEvent::Started { .. } => self.rpc_started.inc(1),
            vox::RpcEvent::Finished {
                outcome, elapsed, ..
            } => {
                self.rpc_finished.inc(1);
                self.rpc_elapsed.record_duration(elapsed);
                if outcome != vox::RpcOutcome::Ok {
                    self.rpc_failed.inc(1);
                    self.registry.event(
                        "vox.rpc.failed",
                        format!("surface={surface} outcome={outcome:?}"),
                    );
                }
            }
        }
    }

    fn channel_event(&self, surface: &'static str, event: vox::ChannelEvent) {
        match event {
            vox::ChannelEvent::Opened { .. } => self.channel_opened.inc(1),
            vox::ChannelEvent::SendStarted { .. } => self.channel_send_started.inc(1),
            vox::ChannelEvent::SendWaitingForCredit { channel } => {
                self.channel_send_waiting_for_credit.inc(1);
                self.registry.event(
                    "vox.channel.waiting_for_credit",
                    channel_detail(surface, channel),
                );
            }
            vox::ChannelEvent::SendFinished {
                channel,
                outcome,
                elapsed,
            } => {
                self.channel_send_finished.inc(1);
                self.channel_send_elapsed.record_duration(elapsed);
                if outcome != vox::ChannelSendOutcome::Sent {
                    self.channel_send_failed.inc(1);
                    self.registry.event(
                        "vox.channel.send_failed",
                        format!("{} outcome={outcome:?}", channel_detail(surface, channel)),
                    );
                }
            }
            vox::ChannelEvent::TrySend { channel, outcome } => {
                self.channel_try_send.inc(1);
                if outcome != vox::ChannelTrySendOutcome::Sent {
                    self.channel_try_send_failed.inc(1);
                    self.registry.event(
                        "vox.channel.try_send_failed",
                        format!("{} outcome={outcome:?}", channel_detail(surface, channel)),
                    );
                }
            }
            vox::ChannelEvent::CreditGranted { amount, .. } => {
                self.channel_credit_grants.inc(1);
                self.channel_credit_granted.inc(u64::from(amount));
            }
            vox::ChannelEvent::ItemReceived { .. } => self.channel_item_received.inc(1),
            vox::ChannelEvent::ItemConsumed { .. } => self.channel_item_consumed.inc(1),
            vox::ChannelEvent::Closed { channel, reason } => {
                self.channel_closed.inc(1);
                self.registry.event(
                    "vox.channel.closed",
                    format!("{} reason={reason:?}", channel_detail(surface, channel)),
                );
            }
            vox::ChannelEvent::Reset { channel, reason } => {
                self.channel_reset.inc(1);
                self.registry.event(
                    "vox.channel.reset",
                    format!("{} reason={reason:?}", channel_detail(surface, channel)),
                );
            }
        }
    }

    fn driver_event(&self, surface: &'static str, event: vox::DriverEvent) {
        match event {
            vox::DriverEvent::ConnectionOpened { connection_id } => {
                self.connections_opened.inc(1);
                self.active_connections.inc(1);
                self.registry.event(
                    "vox.connection.opened",
                    format!("surface={surface} connection_id={connection_id:?}"),
                );
            }
            vox::DriverEvent::ConnectionClosed {
                connection_id,
                reason,
            } => {
                self.connections_closed.inc(1);
                self.active_connections.dec(1);
                self.registry.event(
                    "vox.connection.closed",
                    format!("surface={surface} connection_id={connection_id:?} reason={reason:?}"),
                );
            }
            vox::DriverEvent::RequestStarted { .. } => self.driver_requests_started.inc(1),
            vox::DriverEvent::RequestFinished {
                outcome, elapsed, ..
            } => {
                self.driver_requests_finished.inc(1);
                self.driver_request_elapsed.record_duration(elapsed);
                if outcome != vox::RpcOutcome::Ok {
                    self.driver_requests_failed.inc(1);
                }
            }
            vox::DriverEvent::OutboundQueueFull { connection_id } => {
                self.outbound_queue_full.inc(1);
                self.registry.event(
                    "vox.outbound_queue.full",
                    format!("surface={surface} connection_id={connection_id:?}"),
                );
            }
            vox::DriverEvent::OutboundQueueClosed { connection_id } => {
                self.outbound_queue_closed.inc(1);
                self.registry.event(
                    "vox.outbound_queue.closed",
                    format!("surface={surface} connection_id={connection_id:?}"),
                );
            }
            vox::DriverEvent::FrameRead { bytes, .. } => {
                self.driver_frame_read_bytes.inc(bytes as u64);
            }
            vox::DriverEvent::FrameWritten { bytes, .. } => {
                self.driver_frame_written_bytes.inc(bytes as u64);
            }
            vox::DriverEvent::DecodeError { .. }
            | vox::DriverEvent::EncodeError { .. }
            | vox::DriverEvent::ProtocolError { .. } => {
                self.driver_errors.inc(1);
            }
        }
    }

    fn transport_event(&self, surface: &'static str, event: vox::TransportEvent) {
        match event {
            vox::TransportEvent::FrameRead { bytes, .. } => {
                self.transport_frame_read_bytes.inc(bytes as u64);
            }
            vox::TransportEvent::FrameWritten { bytes, .. } => {
                self.transport_frame_written_bytes.inc(bytes as u64);
            }
            vox::TransportEvent::Closed {
                connection_id,
                reason,
            } => {
                self.transport_closed.inc(1);
                self.registry.event(
                    "vox.transport.closed",
                    format!("surface={surface} connection_id={connection_id:?} reason={reason:?}"),
                );
            }
        }
    }
}

fn channel_detail(surface: &'static str, channel: vox::ChannelEventContext) -> String {
    format!(
        "surface={} connection_id={:?} channel_id={:?} {}",
        surface,
        channel.connection_id,
        channel.channel_id,
        format_channel_debug_inline(channel.debug)
    )
}

impl vox::VoxObserver for VoxObserverLogger {
    fn rpc_event(&self, event: vox::RpcEvent) {
        if let Some(telemetry) = &self.telemetry {
            telemetry.rpc_event(self.surface, event);
        }
        match event {
            vox::RpcEvent::Started {
                service,
                method,
                method_id,
                ..
            } => {
                tracing::trace!(
                    component = self.component,
                    surface = self.surface,
                    pid = ?self.pid,
                    service = ?service,
                    method = ?method,
                    method_id = ?method_id,
                    "vox rpc started"
                );
            }
            vox::RpcEvent::Finished {
                service,
                method,
                method_id,
                outcome,
                elapsed,
                ..
            } => {
                if outcome != vox::RpcOutcome::Ok || elapsed >= SLOW_REQUEST {
                    tracing::info!(
                        component = self.component,
                        surface = self.surface,
                        pid = ?self.pid,
                        service = ?service,
                        method = ?method,
                        method_id = ?method_id,
                        outcome = ?outcome,
                        elapsed = ?elapsed,
                        "vox rpc finished"
                    );
                } else {
                    tracing::trace!(
                        component = self.component,
                        surface = self.surface,
                        pid = ?self.pid,
                        service = ?service,
                        method = ?method,
                        method_id = ?method_id,
                        outcome = ?outcome,
                        elapsed = ?elapsed,
                        "vox rpc finished"
                    );
                }
            }
        }
    }

    fn channel_event(&self, event: vox::ChannelEvent) {
        if let Some(telemetry) = &self.telemetry {
            telemetry.channel_event(self.surface, event);
        }
        match event {
            vox::ChannelEvent::Opened {
                channel,
                direction,
                initial_credit,
            } => {
                tracing::info!(
                    component = self.component,
                    surface = self.surface,
                    pid = ?self.pid,
                    connection_id = ?channel.connection_id,
                    channel_id = ?channel.channel_id,
                    channel_debug = ?channel.debug,
                    direction = ?direction,
                    initial_credit,
                    "vox channel opened"
                );
            }
            vox::ChannelEvent::SendWaitingForCredit { channel } => {
                tracing::info!(
                    component = self.component,
                    surface = self.surface,
                    pid = ?self.pid,
                    connection_id = ?channel.connection_id,
                    channel_id = ?channel.channel_id,
                    channel_debug = ?channel.debug,
                    "vox channel waiting for credit"
                );
            }
            vox::ChannelEvent::SendFinished {
                channel,
                outcome,
                elapsed,
            } => {
                if outcome != vox::ChannelSendOutcome::Sent || elapsed >= SLOW_CHANNEL_SEND {
                    tracing::info!(
                        component = self.component,
                        surface = self.surface,
                        pid = ?self.pid,
                        connection_id = ?channel.connection_id,
                        channel_id = ?channel.channel_id,
                        channel_debug = ?channel.debug,
                        outcome = ?outcome,
                        elapsed = ?elapsed,
                        "vox channel send finished"
                    );
                } else {
                    tracing::trace!(
                        component = self.component,
                        surface = self.surface,
                        pid = ?self.pid,
                        connection_id = ?channel.connection_id,
                        channel_id = ?channel.channel_id,
                        channel_debug = ?channel.debug,
                        outcome = ?outcome,
                        elapsed = ?elapsed,
                        "vox channel send finished"
                    );
                }
            }
            vox::ChannelEvent::TrySend { channel, outcome } => {
                if outcome != vox::ChannelTrySendOutcome::Sent {
                    tracing::warn!(
                        component = self.component,
                        surface = self.surface,
                        pid = ?self.pid,
                        connection_id = ?channel.connection_id,
                        channel_id = ?channel.channel_id,
                        channel_debug = ?channel.debug,
                        outcome = ?outcome,
                        "vox channel try_send failed"
                    );
                }
            }
            vox::ChannelEvent::Closed { channel, reason } => {
                tracing::info!(
                    component = self.component,
                    surface = self.surface,
                    pid = ?self.pid,
                    connection_id = ?channel.connection_id,
                    channel_id = ?channel.channel_id,
                    channel_debug = ?channel.debug,
                    reason = ?reason,
                    "vox channel closed"
                );
            }
            vox::ChannelEvent::Reset { channel, reason } => {
                tracing::warn!(
                    component = self.component,
                    surface = self.surface,
                    pid = ?self.pid,
                    connection_id = ?channel.connection_id,
                    channel_id = ?channel.channel_id,
                    channel_debug = ?channel.debug,
                    reason = ?reason,
                    "vox channel reset"
                );
            }
            vox::ChannelEvent::SendStarted { channel } => {
                tracing::trace!(
                    component = self.component,
                    surface = self.surface,
                    pid = ?self.pid,
                    connection_id = ?channel.connection_id,
                    channel_id = ?channel.channel_id,
                    channel_debug = ?channel.debug,
                    "vox channel send started"
                );
            }
            vox::ChannelEvent::CreditGranted { channel, amount } => {
                tracing::trace!(
                    component = self.component,
                    surface = self.surface,
                    pid = ?self.pid,
                    connection_id = ?channel.connection_id,
                    channel_id = ?channel.channel_id,
                    channel_debug = ?channel.debug,
                    amount,
                    "vox channel credit granted"
                );
            }
            vox::ChannelEvent::ItemReceived { channel } => {
                tracing::trace!(
                    component = self.component,
                    surface = self.surface,
                    pid = ?self.pid,
                    connection_id = ?channel.connection_id,
                    channel_id = ?channel.channel_id,
                    channel_debug = ?channel.debug,
                    "vox channel item received"
                );
            }
            vox::ChannelEvent::ItemConsumed { channel } => {
                tracing::trace!(
                    component = self.component,
                    surface = self.surface,
                    pid = ?self.pid,
                    connection_id = ?channel.connection_id,
                    channel_id = ?channel.channel_id,
                    channel_debug = ?channel.debug,
                    "vox channel item consumed"
                );
            }
        }
    }

    fn driver_event(&self, event: vox::DriverEvent) {
        if let Some(telemetry) = &self.telemetry {
            telemetry.driver_event(self.surface, event);
        }
        match event {
            vox::DriverEvent::ConnectionOpened { connection_id } => {
                tracing::info!(
                    component = self.component,
                    surface = self.surface,
                    pid = ?self.pid,
                    connection_id = ?connection_id,
                    "vox connection opened"
                );
            }
            vox::DriverEvent::ConnectionClosed {
                connection_id,
                reason,
            } => {
                tracing::info!(
                    component = self.component,
                    surface = self.surface,
                    pid = ?self.pid,
                    connection_id = ?connection_id,
                    reason = ?reason,
                    "vox connection closed"
                );
            }
            vox::DriverEvent::RequestFinished {
                connection_id,
                request_id,
                outcome,
                elapsed,
            } => {
                if outcome != vox::RpcOutcome::Ok || elapsed >= SLOW_REQUEST {
                    tracing::info!(
                        component = self.component,
                        surface = self.surface,
                        pid = ?self.pid,
                        connection_id = ?connection_id,
                        request_id = ?request_id,
                        outcome = ?outcome,
                        elapsed = ?elapsed,
                        "vox request finished"
                    );
                } else {
                    tracing::trace!(
                        component = self.component,
                        surface = self.surface,
                        pid = ?self.pid,
                        connection_id = ?connection_id,
                        request_id = ?request_id,
                        outcome = ?outcome,
                        elapsed = ?elapsed,
                        "vox request finished"
                    );
                }
            }
            vox::DriverEvent::OutboundQueueFull { connection_id } => {
                tracing::warn!(
                    component = self.component,
                    surface = self.surface,
                    pid = ?self.pid,
                    connection_id = ?connection_id,
                    "vox outbound queue full"
                );
            }
            vox::DriverEvent::OutboundQueueClosed { connection_id } => {
                tracing::warn!(
                    component = self.component,
                    surface = self.surface,
                    pid = ?self.pid,
                    connection_id = ?connection_id,
                    "vox outbound queue closed"
                );
            }
            vox::DriverEvent::DecodeError {
                connection_id,
                kind,
            } => {
                tracing::warn!(
                    component = self.component,
                    surface = self.surface,
                    pid = ?self.pid,
                    connection_id = ?connection_id,
                    kind = ?kind,
                    "vox decode error"
                );
            }
            vox::DriverEvent::EncodeError {
                connection_id,
                kind,
            } => {
                tracing::warn!(
                    component = self.component,
                    surface = self.surface,
                    pid = ?self.pid,
                    connection_id = ?connection_id,
                    kind = ?kind,
                    "vox encode error"
                );
            }
            vox::DriverEvent::ProtocolError {
                connection_id,
                kind,
            } => {
                tracing::warn!(
                    component = self.component,
                    surface = self.surface,
                    pid = ?self.pid,
                    connection_id = ?connection_id,
                    kind = ?kind,
                    "vox protocol error"
                );
            }
            vox::DriverEvent::RequestStarted {
                connection_id,
                request_id,
                method_id,
            } => {
                tracing::trace!(
                    component = self.component,
                    surface = self.surface,
                    pid = ?self.pid,
                    connection_id = ?connection_id,
                    request_id = ?request_id,
                    method_id = ?method_id,
                    "vox request started"
                );
            }
            vox::DriverEvent::FrameRead {
                connection_id,
                bytes,
            } => {
                tracing::trace!(
                    component = self.component,
                    surface = self.surface,
                    pid = ?self.pid,
                    connection_id = ?connection_id,
                    bytes,
                    "vox frame read"
                );
            }
            vox::DriverEvent::FrameWritten {
                connection_id,
                bytes,
            } => {
                tracing::trace!(
                    component = self.component,
                    surface = self.surface,
                    pid = ?self.pid,
                    connection_id = ?connection_id,
                    bytes,
                    "vox frame written"
                );
            }
        }
    }

    fn transport_event(&self, event: vox::TransportEvent) {
        if let Some(telemetry) = &self.telemetry {
            telemetry.transport_event(self.surface, event);
        }
        tracing::trace!(
            component = self.component,
            surface = self.surface,
            pid = ?self.pid,
            event = ?event,
            "vox transport event"
        );
    }
}
