import Foundation
@preconcurrency import NIOCore
import Observation
import SwiftUI
import VoxRuntime

@Observable
@MainActor
final class AppModel {
    var paused: Bool = false {
        didSet {
            guard oldValue != paused else { return }
            let target = paused
            Task { [weak self] in
                guard case .ready(let client) = await self?.service.state else { return }
                do {
                    try await client.setPaused(paused: target)
                    NSLog("stax: setPaused(%@)", target ? "true" : "false")
                } catch {
                    NSLog("stax: setPaused failed: %@", "\(error)")
                }
            }
        }
    }

    /// `nil` means all threads.
    var threadFilter: Int? = nil
    /// Quoted (`"foo"`) → exact substring. Slashed (`/foo/`) → regex.
    /// Plain text → fuzzy substring.
    var searchQuery: String = ""

    /// `nil` → top pane shows the flame graph. Non-nil → top pane shows the
    /// call graph centered on the focused function.
    var focusedFunctionId: FunctionEntry.ID? = nil {
        didSet {
            guard oldValue != focusedFunctionId else { return }
            restartAnnotatedSubscription()
            restartNeighborsSubscription()
        }
    }

    /// Live disassembly + source view for the focused function.
    /// Populated by `subscribe_annotated` while a function is focused;
    /// nil while the flame graph is the top pane.
    var annotated: AnnotatedView? = nil

    /// Time-bucketed activity for the minimap. Always relative to
    /// the full recording (no filter); brush selection happens on
    /// top of the unfiltered timeline.
    var timeline: TimelineUpdate? = nil

    enum CPUMode: String, CaseIterable, Identifiable {
        case onCPU = "on-cpu"
        case offCPU = "off-cpu"
        case wall = "wall"
        var id: String { rawValue }

        var fakeStat: String {
            switch self {
            case .onCPU:  "3.0ms"
            case .offCPU: "1.70s"
            case .wall:   "1.71s"
            }
        }
    }
    var cpuMode: CPUMode = .onCPU

    enum EventMode: String, CaseIterable, Identifiable {
        case ipc = "ipc"
        case l1d = "l1d"
        case brMiss = "br-miss"
        var id: String { rawValue }

        var fakeStat: String {
            switch self {
            case .ipc:    "1.42"
            case .l1d:    "32k"
            case .brMiss: "1.1k"
            }
        }
    }
    var eventMode: EventMode? = .ipc

    enum Category: String, CaseIterable, Identifiable {
        case main, dylib, system, other
        var id: String { rawValue }

        var color: Color {
            switch self {
            case .main:   Color(red: 0.96, green: 0.78, blue: 0.27) // amber
            case .dylib:  Color(red: 0.36, green: 0.78, blue: 0.85) // cyan
            case .system: Color(red: 0.95, green: 0.55, blue: 0.43) // coral
            case .other:  Color(red: 0.74, green: 0.56, blue: 0.91) // violet
            }
        }

        var fakeCount: Int {
            switch self {
            case .main:   18
            case .dylib:  24
            case .system: 6
            case .other:  2
            }
        }
    }
    var categories: Set<Category> = [.main, .dylib]

    struct ThreadInfo: Identifiable, Hashable {
        var id: Int { tid }
        let tid: Int
        let name: String?
        let onCPU: TimeInterval

        var displayName: String {
            name ?? "[\(tid)]"
        }
    }
    var threads: [ThreadInfo] = []

    /// Threads sorted by on-CPU time, descending.
    var threadsSorted: [ThreadInfo] {
        threads.sorted { $0.onCPU > $1.onCPU }
    }

    var totalThreadOnCPU: TimeInterval {
        threads.reduce(0) { $0 + $1.onCPU }
    }

    var maxThreadOnCPU: TimeInterval {
        max(0.001, threads.map(\.onCPU).max() ?? 0)
    }

    func thread(forTid tid: Int) -> ThreadInfo? {
        threads.first { $0.tid == tid }
    }

    // Fake stats for the bottom status bar.
    var onCPUTime: TimeInterval = 0.003
    var offCPUTime: TimeInterval = 1.70
    var symbolCount: Int = 50

    struct FunctionEntry: Identifiable, Hashable {
        let id = UUID()
        /// AVMA of (or near) this symbol — used to drill in via
        /// `subscribe_annotated`.
        let address: UInt64
        let name: String
        let binary: String
        let kind: SymbolKind
        let selfTime: TimeInterval
        let totalTime: TimeInterval
    }
    struct FamilyMember: Identifiable, Hashable {
        let id = UUID()
        let name: String
        let binary: String
        let kind: SymbolKind
        let totalTime: TimeInterval
        let callCount: Int
    }
    var familyCallers: [FamilyMember] = [
        .init(name: "IOGPUCommandQueueSubmitCommandBuffers",         binary: "IOGPU",                   kind: .c, totalTime: 0.0003062, callCount: 1),
        .init(name: "start_wqthread",                                binary: "libsystem_pthread.dylib", kind: .c, totalTime: 0.0003062, callCount: 1),
        .init(name: "_pthread_wqthread",                             binary: "libsystem_pthread.dylib", kind: .c, totalTime: 0.0003062, callCount: 1),
        .init(name: "_dispatch_workloop_worker_thread",              binary: "libdispatch.dylib",       kind: .c, totalTime: 0.0003062, callCount: 1),
        .init(name: "_dispatch_root_queue_drain_deferred_wlh",       binary: "libdispatch.dylib",       kind: .c, totalTime: 0.0003062, callCount: 1),
        .init(name: "_dispatch_lane_invoke",                         binary: "libdispatch.dylib",       kind: .c, totalTime: 0.0003062, callCount: 1),
        .init(name: "_dispatch_lane_serial_drain",                   binary: "libdispatch.dylib",       kind: .c, totalTime: 0.0003062, callCount: 1),
        .init(name: "_dispatch_source_invoke",                       binary: "libdispatch.dylib",       kind: .c, totalTime: 0.0003062, callCount: 1),
    ]
    var familyFocused: FamilyMember = .init(
        name: "_dispatch_source_latch_and_call",
        binary: "libdispatch.dylib",
        kind: .c,
        totalTime: 0.0003062,
        callCount: 1
    )
    var familyCallees: [FamilyMember] = [
        .init(name: "_dispatch_continuation_pop",                                  binary: "libdispatch.dylib", kind: .c,    totalTime: 0.0000180, callCount: 1),
        .init(name: "_dispatch_client_callout",                                    binary: "libdispatch.dylib", kind: .c,    totalTime: 0.0001800, callCount: 1),
        .init(name: "-[_MTLCommandQueue _submitAvailableCommandBuffers]",          binary: "Metal",             kind: .objc, totalTime: 0.0001500, callCount: 1),
        .init(name: "-[IOGPUMetalCommandQueue submitCommandBuffers:count:]",       binary: "IOGPU",             kind: .objc, totalTime: 0.0001000, callCount: 2),
        .init(name: "-[IOGPUMetalCommandQueue _submitCommandBuffers:count:]",      binary: "IOGPU",             kind: .objc, totalTime: 0.0000800, callCount: 2),
        .init(name: "iokit_user_client_trap",                                      binary: "IOKit",             kind: .c,    totalTime: 0.0000750, callCount: 4),
    ]

    enum IntervalReason: String, CaseIterable, Identifiable, Hashable {
        case ipc, read, write, ready, connect, idle, other
        var id: String { rawValue }
        var color: Color {
            switch self {
            case .ipc:     Color(red: 0.74, green: 0.56, blue: 0.91)
            case .read:    Color(red: 0.36, green: 0.65, blue: 0.95)
            case .write:   Color(red: 0.36, green: 0.78, blue: 0.85)
            case .ready:   Color(red: 0.55, green: 0.82, blue: 0.45)
            case .connect: Color(red: 0.95, green: 0.65, blue: 0.30)
            case .idle:    Color(red: 0.50, green: 0.50, blue: 0.55)
            case .other:   Color(red: 0.95, green: 0.55, blue: 0.43)
            }
        }
        var fakeStat: TimeInterval {
            switch self {
            case .ipc:     0.0000197
            case .read:    0.0000105
            case .write:   0.0000047
            case .ready:   0.0000093
            case .connect: 0.0000163
            case .idle:    0.1999
            case .other:   0.5874
            }
        }
    }

    struct Interval: Identifiable, Hashable {
        let id = UUID()
        let start: TimeInterval
        let duration: TimeInterval
        let reason: IntervalReason
        let tid: Int
        let wokenBy: Int?
    }
    var intervals: [Interval] = {
        let durations: [TimeInterval] = [
            0.1387, 0.000000113, 0.0000013, 0.0000043, 0.0000027,
            0.0000044, 0.0000017, 0.0000053, 0.0000023, 0.0000057,
            0.0000031, 0.0000040, 0.000000595, 0.0000038, 0.0000036,
            0.0000036, 0.0000019, 0.0000060,
        ]
        return durations.map {
            Interval(start: 0.254, duration: $0, reason: .other, tid: 6360176, wokenBy: nil)
        }
    }()
    var intervalsTotalCount: Int = 20577
    var intervalsTotalDuration: TimeInterval = 0.7874

    var functions: [FunctionEntry] = []

    // MARK: - Live data plumbing

    /// Why a connection isn't live (or empty when everything's fine).
    var connectionStatus: String = "disconnected"
    private let service = ProfilerService()
    private var streamTasks: [Task<Void, Never>] = []
    /// Restartable subscriptions: cancel + relaunch every time
    /// `focusedFunctionId` changes. Both hold nil while the flame
    /// graph is the top pane.
    private var annotatedTask: Task<Void, Never>?
    private var neighborsTask: Task<Void, Never>?

    /// Connect to stax-server, then start subscriptions that drive
    /// live-data fields (`threads`, `functions`, total stats, …).
    /// Idempotent.
    func start() async {
        guard streamTasks.isEmpty else { return }
        connectionStatus = "connecting"
        await service.connect()
        switch service.state {
        case .ready(let client):
            connectionStatus = "connected"

            // Smoke-test the unary path on the way in. If this fails
            // the streaming subscriptions won't work either, so we
            // surface the failure up front.
            do {
                let total = try await client.totalOnCpuNs()
                NSLog("stax: totalOnCpuNs = %llu", total)
                self.onCPUTime = TimeInterval(total) / 1_000_000_000
            } catch {
                NSLog("stax: totalOnCpuNs failed: %@", "\(error)")
                connectionStatus = "totalOnCpuNs failed"
                return
            }

            streamTasks.append(Task { [weak self] in
                await self?.runThreadsSubscription(client: client)
            })
            streamTasks.append(Task { [weak self] in
                await self?.runTopSubscription(client: client)
            })
            streamTasks.append(Task { [weak self] in
                await self?.runTimelineSubscription(client: client)
            })
        case .failed(let why):
            connectionStatus = why
        case .idle, .connecting:
            connectionStatus = "stuck"
        }
    }

    private func runThreadsSubscription(client: ProfilerClient) async {
        let (tx, rx) = channel(
            serialize: { (val: ThreadsUpdate, buf: inout ByteBuffer) in
                encodeThreadsUpdate(val, into: &buf)
            },
            deserialize: { (buf: inout ByteBuffer) in
                try decodeThreadsUpdate(from: &buf)
            }
        )

        Task {
            do {
                try await client.subscribeThreads(output: tx)
                NSLog("stax: subscribeThreads call returned")
            } catch {
                NSLog("stax: subscribeThreads call failed: %@", "\(error)")
            }
        }

        do {
            var count = 0
            for try await update in rx {
                count += 1
                NSLog("stax: threads update #%d (%d threads)", count, update.threads.count)
                self.threads = update.threads.map { wire in
                    ThreadInfo(
                        tid: Int(wire.tid),
                        name: wire.name,
                        onCPU: TimeInterval(wire.onCpuNs) / 1_000_000_000
                    )
                }
            }
            NSLog("stax: threads stream ended")
        } catch {
            NSLog("stax: threads stream error: %@", "\(error)")
        }
    }

    private func runTopSubscription(client: ProfilerClient) async {
        let (tx, rx) = channel(
            serialize: { (val: TopUpdate, buf: inout ByteBuffer) in
                encodeTopUpdate(val, into: &buf)
            },
            deserialize: { (buf: inout ByteBuffer) in
                try decodeTopUpdate(from: &buf)
            }
        )

        let params = ViewParams(
            tid: model_tidFilterAsU32(),
            filter: LiveFilter(timeRange: nil, excludeSymbols: [])
        )

        Task {
            do {
                try await client.subscribeTop(
                    limit: 100,
                    sort: .bySelf,
                    params: params,
                    output: tx
                )
                NSLog("stax: subscribeTop call returned")
            } catch {
                NSLog("stax: subscribeTop call failed: %@", "\(error)")
            }
        }

        do {
            var count = 0
            for try await update in rx {
                count += 1
                NSLog(
                    "stax: top update #%d (%d entries, total on-cpu=%llu ns)",
                    count,
                    update.entries.count,
                    update.totalOnCpuNs
                )
                self.functions = update.entries.map { wire in
                    let name =
                        wire.functionName
                        ?? String(format: "0x%llx", wire.address)
                    let binary = wire.binary ?? "(no binary)"
                    return FunctionEntry(
                        address: wire.address,
                        name: name,
                        binary: binary,
                        kind: symbolKind(forLanguage: wire.language),
                        selfTime: TimeInterval(wire.selfOnCpuNs) / 1_000_000_000,
                        totalTime: TimeInterval(wire.totalOnCpuNs) / 1_000_000_000
                    )
                }
                self.symbolCount = update.entries.count
                self.onCPUTime = TimeInterval(update.totalOnCpuNs) / 1_000_000_000
            }
            NSLog("stax: top stream ended")
        } catch {
            NSLog("stax: top stream error: %@", "\(error)")
        }
    }

    private func runTimelineSubscription(client: ProfilerClient) async {
        let (tx, rx) = channel(
            serialize: { (val: TimelineUpdate, buf: inout ByteBuffer) in
                encodeTimelineUpdate(val, into: &buf)
            },
            deserialize: { (buf: inout ByteBuffer) in
                try decodeTimelineUpdate(from: &buf)
            }
        )

        // The timeline is always relative to the whole recording —
        // brush selection is applied client-side. `tid: nil` means
        // "all threads".
        Task {
            do {
                try await client.subscribeTimeline(tid: nil, output: tx)
                NSLog("stax: subscribeTimeline call returned")
            } catch {
                NSLog("stax: subscribeTimeline call failed: %@", "\(error)")
            }
        }

        do {
            var count = 0
            for try await update in rx {
                count += 1
                NSLog(
                    "stax: timeline update #%d (%d buckets, duration=%llu ns)",
                    count,
                    update.buckets.count,
                    update.recordingDurationNs
                )
                self.timeline = update
            }
            NSLog("stax: timeline stream ended")
        } catch {
            NSLog("stax: timeline stream error: %@", "\(error)")
        }
    }

    private func model_tidFilterAsU32() -> UInt32? {
        guard let tid = threadFilter else { return nil }
        return UInt32(exactly: tid)
    }

    /// Cancel any in-flight annotated subscription and start a new
    /// one for the currently-focused function (if any).
    private func restartAnnotatedSubscription() {
        annotatedTask?.cancel()
        annotatedTask = nil
        annotated = nil
        guard
            let id = focusedFunctionId,
            let fn = functions.first(where: { $0.id == id })
        else { return }
        guard case .ready(let client) = service.state else { return }

        let address = fn.address
        annotatedTask = Task { [weak self] in
            await self?.runAnnotatedSubscription(client: client, address: address)
        }
    }

    /// Cancel any in-flight neighbors subscription and start a new
    /// one for the currently-focused function (if any). The result
    /// populates `familyCallers` / `familyFocused` / `familyCallees`,
    /// which the call-graph view reads.
    private func restartNeighborsSubscription() {
        neighborsTask?.cancel()
        neighborsTask = nil
        guard
            let id = focusedFunctionId,
            let fn = functions.first(where: { $0.id == id })
        else { return }
        guard case .ready(let client) = service.state else { return }

        let address = fn.address
        neighborsTask = Task { [weak self] in
            await self?.runNeighborsSubscription(client: client, address: address)
        }
    }

    private func runNeighborsSubscription(
        client: ProfilerClient,
        address: UInt64
    ) async {
        let (tx, rx) = channel(
            serialize: { (val: NeighborsUpdate, buf: inout ByteBuffer) in
                encodeNeighborsUpdate(val, into: &buf)
            },
            deserialize: { (buf: inout ByteBuffer) in
                try decodeNeighborsUpdate(from: &buf)
            }
        )

        let params = ViewParams(
            tid: model_tidFilterAsU32(),
            filter: LiveFilter(timeRange: nil, excludeSymbols: [])
        )

        Task {
            do {
                try await client.subscribeNeighbors(
                    address: address,
                    params: params,
                    output: tx
                )
                NSLog("stax: subscribeNeighbors call returned")
            } catch {
                NSLog("stax: subscribeNeighbors call failed: %@", "\(error)")
            }
        }

        do {
            for try await update in rx {
                if Task.isCancelled { break }
                NSLog(
                    "stax: neighbors update (%d callers, %d callees)",
                    update.callersTree.children.count,
                    update.calleesTree.children.count
                )
                applyNeighbors(update)
            }
        } catch {
            NSLog("stax: neighbors stream error: %@", "\(error)")
        }
    }

    private func applyNeighbors(_ update: NeighborsUpdate) {
        let strings = update.strings

        func resolveOptional(_ idx: UInt32?) -> String? {
            guard let i = idx, Int(i) < strings.count else { return nil }
            return strings[Int(i)]
        }
        func resolveLanguage(_ idx: UInt32) -> String {
            Int(idx) < strings.count ? strings[Int(idx)] : ""
        }
        func nodeToMember(_ node: FlameNode) -> FamilyMember {
            let name =
                resolveOptional(node.functionName)
                ?? String(format: "0x%llx", node.address)
            let binary = resolveOptional(node.binary) ?? "(no binary)"
            return FamilyMember(
                name: name,
                binary: binary,
                kind: symbolKind(forLanguage: resolveLanguage(node.language)),
                totalTime: TimeInterval(node.onCpuNs) / 1_000_000_000,
                callCount: max(1, Int(node.petSamples))
            )
        }

        let focusedAddress = update.callersTree.address
        let focusedName =
            resolveOptional(update.functionName)
            ?? String(format: "0x%llx", focusedAddress)
        let focusedBinary = resolveOptional(update.binary) ?? "(no binary)"

        self.familyFocused = FamilyMember(
            name: focusedName,
            binary: focusedBinary,
            kind: symbolKind(forLanguage: resolveLanguage(update.language)),
            totalTime: TimeInterval(update.ownOnCpuNs) / 1_000_000_000,
            callCount: max(1, Int(update.ownPetSamples))
        )
        self.familyCallers = update.callersTree.children.map(nodeToMember)
        self.familyCallees = update.calleesTree.children.map(nodeToMember)
    }

    private func runAnnotatedSubscription(
        client: ProfilerClient,
        address: UInt64
    ) async {
        let (tx, rx) = channel(
            serialize: { (val: AnnotatedView, buf: inout ByteBuffer) in
                encodeAnnotatedView(val, into: &buf)
            },
            deserialize: { (buf: inout ByteBuffer) in
                try decodeAnnotatedView(from: &buf)
            }
        )

        let params = ViewParams(
            tid: model_tidFilterAsU32(),
            filter: LiveFilter(timeRange: nil, excludeSymbols: [])
        )

        Task {
            do {
                try await client.subscribeAnnotated(
                    address: address,
                    params: params,
                    output: tx
                )
                NSLog("stax: subscribeAnnotated call returned")
            } catch {
                NSLog("stax: subscribeAnnotated call failed: %@", "\(error)")
            }
        }

        do {
            for try await update in rx {
                if Task.isCancelled { break }
                NSLog(
                    "stax: annotated update for %@ (%d lines)",
                    update.functionName,
                    update.lines.count
                )
                self.annotated = update
            }
        } catch {
            NSLog("stax: annotated stream error: %@", "\(error)")
        }
    }
}

private func symbolKind(forLanguage language: String) -> SymbolKind {
    switch language.lowercased() {
    case "rust":           .rust
    case "c":              .c
    case "cpp", "c++":     .cpp
    case "swift":          .swift
    case "objc", "objective-c", "objectivec": .objc
    default:               .unknown
    }
}
