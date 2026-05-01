import Foundation
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

    /// Stored AVMA we're currently focused on. Drives the annotated +
    /// neighbors subscriptions; can be set both by clicking a row in
    /// the function table (via `focusedFunctionId.didSet` mirror)
    /// and by clicking a frame in the flame graph (which doesn't go
    /// through the table at all).
    var focusedAddress: UInt64? = nil {
        didSet {
            guard oldValue != focusedAddress else { return }
            restartAnnotatedSubscription()
            restartNeighborsSubscription()
            restartCfgSubscription()
        }
    }

    /// What to render in the NavHeader / call-graph nodes for the
    /// currently focused address. Can come from either a FunctionEntry
    /// (table click) or a FlameNode (flame click); kept normalized so
    /// the view layer doesn't care which side originated the focus.
    var focusedDisplay: FocusedDisplay? = nil

    struct FocusedDisplay: Hashable {
        let name: String
        let binary: String
        let kind: SymbolKind
    }

    /// `nil` → top pane shows the flame graph. Non-nil → top pane shows the
    /// call graph centered on the focused function. Kept in sync with
    /// `focusedAddress`/`focusedDisplay` via didSet.
    var focusedFunctionId: FunctionEntry.ID? = nil {
        didSet {
            guard oldValue != focusedFunctionId else { return }
            if let id = focusedFunctionId,
                let fn = functions.first(where: { $0.id == id })
            {
                let display = FocusedDisplay(
                    name: fn.name, binary: fn.binary, kind: fn.kind)
                if focusedDisplay != display { focusedDisplay = display }
                if focusedAddress != fn.address { focusedAddress = fn.address }
            } else {
                if focusedDisplay != nil { focusedDisplay = nil }
                if focusedAddress != nil { focusedAddress = nil }
            }
        }
    }

    /// Focus on the given flame-graph frame. The matching table row
    /// is highlighted if the address happens to be in the current
    /// top-N; otherwise the table stays unselected but the call graph
    /// + disassembly still drive off `focusedAddress`.
    func focusOnFlameNode(_ node: FlameNode, strings: [String]) {
        func resolve(_ idx: UInt32?) -> String? {
            guard let i = idx, Int(i) < strings.count else { return nil }
            return strings[Int(i)]
        }
        let name =
            resolve(node.functionName)
            ?? String(format: "0x%llx", node.address)
        let binary = resolve(node.binary) ?? "(no binary)"
        let lang =
            Int(node.language) < strings.count
            ? strings[Int(node.language)]
            : ""
        let display = FocusedDisplay(
            name: name, binary: binary, kind: symbolKind(forLanguage: lang))

        if focusedDisplay != display { focusedDisplay = display }
        if focusedAddress != node.address { focusedAddress = node.address }
        let matchingId = functions.first(where: { $0.address == node.address })?.id
        if focusedFunctionId != matchingId { focusedFunctionId = matchingId }
    }

    /// Clear the focus and return the top pane to the flame graph.
    func clearFocus() {
        if focusedFunctionId != nil { focusedFunctionId = nil }
        if focusedAddress != nil { focusedAddress = nil }
        if focusedDisplay != nil { focusedDisplay = nil }
    }

    /// Live disassembly + source view for the focused function.
    /// Populated by `subscribe_annotated` while a function is focused;
    /// nil while the flame graph is the top pane.
    var annotated: AnnotatedView? = nil

    /// Live CFG (basic blocks + edges) for the focused function.
    /// Populated by `subscribe_cfg` while a function is focused;
    /// nil otherwise. Heatmap stats live on each block's `lines`.
    var cfg: CfgUpdate? = nil

    /// Time-bucketed activity for the minimap. Always relative to
    /// the full recording (no filter); brush selection happens on
    /// top of the unfiltered timeline.
    var timeline: TimelineUpdate? = nil

    /// The (filtered) flame graph backing the top pane when no
    /// function is focused. Held in the wire shape; the renderer
    /// resolves string-table indices on draw.
    var flamegraph: FlamegraphUpdate? = nil

    /// Callers + callees trees centered on the focused function.
    /// Held verbatim so the call-graph layout can walk the FlameNode
    /// trees and resolve `update.strings` on render.
    var neighbors: NeighborsUpdate? = nil

    enum CPUMode: String, CaseIterable, Identifiable {
        case onCPU = "on-cpu"
        case offCPU = "off-cpu"
        case wall = "wall"
        var id: String { rawValue }
    }
    var cpuMode: CPUMode = .onCPU

    enum EventMode: String, CaseIterable, Identifiable {
        case ipc = "ipc"
        case l1d = "l1d"
        case brMiss = "br-miss"
        var id: String { rawValue }
    }
    var eventMode: EventMode? = .ipc

    enum Category: String, CaseIterable, Identifiable {
        case main, dylib, system, other
        var id: String { rawValue }

        var color: Color {
            switch self {
            case .main: Color(red: 0.96, green: 0.78, blue: 0.27)  // amber
            case .dylib: Color(red: 0.36, green: 0.78, blue: 0.85)  // cyan
            case .system: Color(red: 0.95, green: 0.55, blue: 0.43)  // coral
            case .other: Color(red: 0.74, green: 0.56, blue: 0.91)  // violet
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

    // Status bar totals, populated by runTopSubscription.
    var onCPUTime: TimeInterval = 0
    var offCPUTime: TimeInterval = 0
    var symbolCount: Int = 0

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
    var familyCallers: [FamilyMember] = []
    var familyFocused: FamilyMember = .init(
        name: "", binary: "", kind: .unknown, totalTime: 0, callCount: 0)
    var familyCallees: [FamilyMember] = []

    enum IntervalReason: String, CaseIterable, Identifiable, Hashable {
        case ipc, read, write, ready, connect, idle, other
        var id: String { rawValue }
        var color: Color {
            switch self {
            case .ipc: Color(red: 0.74, green: 0.56, blue: 0.91)
            case .read: Color(red: 0.36, green: 0.65, blue: 0.95)
            case .write: Color(red: 0.36, green: 0.78, blue: 0.85)
            case .ready: Color(red: 0.55, green: 0.82, blue: 0.45)
            case .connect: Color(red: 0.95, green: 0.65, blue: 0.30)
            case .idle: Color(red: 0.50, green: 0.50, blue: 0.55)
            case .other: Color(red: 0.95, green: 0.55, blue: 0.43)
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
    var intervals: [Interval] = []
    var intervalsTotalCount: Int = 0
    var intervalsTotalDuration: TimeInterval = 0

    var functions: [FunctionEntry] = []

    // MARK: - Live data plumbing

    /// Why a connection isn't live (or empty when everything's fine).
    var connectionStatus: String = "disconnected"
    /// Per-view update counters surfaced in the status bar so you
    /// can see whether anything is actually refreshing without
    /// scraping the unified log.
    var streamStats = StreamStats()
    private let service = ProfilerService()
    private var streamTasks: [Task<Void, Never>] = []

    struct StreamStats: Hashable {
        var threads: Int = 0
        var top: Int = 0
        var flamegraph: Int = 0
        var timeline: Int = 0
        var neighbors: Int = 0
        var annotated: Int = 0
        var cfg: Int = 0
    }
    /// Restartable subscriptions: cancel + relaunch every time
    /// `focusedFunctionId` changes. All hold nil while the flame
    /// graph is the top pane.
    private var annotatedTask: Task<Void, Never>?
    private var neighborsTask: Task<Void, Never>?
    private var cfgTask: Task<Void, Never>?

    /// Connect to stax-server, then start polling tasks that drive
    /// live-data fields (`threads`, `functions`, total stats, …).
    /// Idempotent.
    func start() async {
        guard streamTasks.isEmpty else { return }
        connectionStatus = "connecting"
        await service.connect()
        switch service.state {
        case .ready(let client):
            connectionStatus = "connected"

            // Spawn all pollers concurrently. Don't gate them on a
            // smoke-test — if any one view is slow, the others should
            // keep refreshing, and the visible counters tell us which
            // ones woke up.
            streamTasks.append(
                Task { [weak self] in
                    await self?.runThreadsSubscription(client: client)
                })
            streamTasks.append(
                Task { [weak self] in
                    await self?.runTopSubscription(client: client)
                })
            streamTasks.append(
                Task { [weak self] in
                    await self?.runTimelineSubscription(client: client)
                })
            streamTasks.append(
                Task { [weak self] in
                    await self?.runFlamegraphSubscription(client: client)
                })

            Task { [weak self] in
                guard let client = await self?.activeClient() else { return }
                do {
                    let total = try await client.totalOnCpuNs()
                    NSLog("stax: totalOnCpuNs = %llu", total)
                    await MainActor.run {
                        self?.onCPUTime = TimeInterval(total) / 1_000_000_000
                    }
                } catch {
                    NSLog("stax: totalOnCpuNs failed: %@", "\(error)")
                }
            }
        case .failed(let why):
            connectionStatus = why
        case .idle, .connecting:
            connectionStatus = "stuck"
        }
    }

    private func runThreadsSubscription(client: ProfilerClient) async {
        var count = 0
        while !Task.isCancelled {
            do {
                let update = try await client.threads()
                count += 1
                NSLog("stax: threads update #%d (%d threads)", count, update.threads.count)
                self.streamStats.threads = count
                self.threads = update.threads.map { wire in
                    ThreadInfo(
                        tid: Int(wire.tid),
                        name: wire.name,
                        onCPU: TimeInterval(wire.onCpuNs) / 1_000_000_000
                    )
                }
            } catch {
                NSLog("stax: threads poll failed: %@", "\(error)")
            }
            try? await Task.sleep(nanoseconds: 500_000_000)
        }
    }

    private func runTopSubscription(client: ProfilerClient) async {
        var count = 0
        while !Task.isCancelled {
            let params = ViewParams(
                tid: model_tidFilterAsU32(),
                filter: LiveFilter(timeRange: nil, excludeSymbols: [])
            )
            do {
                let update = try await client.topUpdate(
                    limit: 100,
                    sort: .bySelf,
                    params: params
                )
                count += 1
                NSLog(
                    "stax: top update #%d (%d entries, total on-cpu=%llu ns)",
                    count,
                    update.entries.count,
                    update.totalOnCpuNs
                )
                self.streamStats.top = count
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
                let oc = update.totalOffCpu
                self.offCPUTime =
                    TimeInterval(
                        oc.idleNs + oc.lockNs + oc.semaphoreNs + oc.ipcNs
                            + oc.ioReadNs + oc.ioWriteNs + oc.readinessNs + oc.sleepNs
                            + oc.connectNs + oc.otherNs
                    ) / 1_000_000_000
            } catch {
                NSLog("stax: top poll failed: %@", "\(error)")
            }
            try? await Task.sleep(nanoseconds: 500_000_000)
        }
    }

    private func runFlamegraphSubscription(client: ProfilerClient) async {
        var count = 0
        while !Task.isCancelled {
            let params = ViewParams(
                tid: model_tidFilterAsU32(),
                filter: LiveFilter(timeRange: nil, excludeSymbols: [])
            )
            do {
                let update = try await client.flamegraph(params: params)
                count += 1
                NSLog(
                    "stax: flamegraph update #%d (%d strings, root.children=%d)",
                    count,
                    update.strings.count,
                    update.root.children.count
                )
                self.streamStats.flamegraph = count
                self.flamegraph = update
            } catch {
                NSLog("stax: flamegraph poll failed: %@", "\(error)")
            }
            try? await Task.sleep(nanoseconds: 500_000_000)
        }
    }

    private func runTimelineSubscription(client: ProfilerClient) async {
        // The timeline is always relative to the whole recording —
        // brush selection is applied client-side. `tid: nil` means
        // "all threads".
        var count = 0
        while !Task.isCancelled {
            do {
                let update = try await client.timeline(tid: nil)
                count += 1
                NSLog(
                    "stax: timeline update #%d (%d buckets, duration=%llu ns)",
                    count,
                    update.buckets.count,
                    update.recordingDurationNs
                )
                self.streamStats.timeline = count
                self.timeline = update
            } catch {
                NSLog("stax: timeline poll failed: %@", "\(error)")
            }
            try? await Task.sleep(nanoseconds: 500_000_000)
        }
    }

    private func activeClient() -> ProfilerClient? {
        if case .ready(let client) = service.state { return client }
        return nil
    }

    private func model_tidFilterAsU32() -> UInt32? {
        guard let tid = threadFilter else { return nil }
        return UInt32(exactly: tid)
    }

    /// Cancel any in-flight annotated subscription and start a new
    /// one for the currently-focused address (if any).
    private func restartAnnotatedSubscription() {
        annotatedTask?.cancel()
        annotatedTask = nil
        annotated = nil
        guard let address = focusedAddress else { return }
        guard case .ready(let client) = service.state else { return }

        annotatedTask = Task { [weak self] in
            await self?.runAnnotatedSubscription(client: client, address: address)
        }
    }

    /// Cancel any in-flight CFG subscription and start a new one for
    /// the currently-focused address (if any).
    private func restartCfgSubscription() {
        cfgTask?.cancel()
        cfgTask = nil
        cfg = nil
        guard let address = focusedAddress else { return }
        guard case .ready(let client) = service.state else { return }

        cfgTask = Task { [weak self] in
            await self?.runCfgSubscription(client: client, address: address)
        }
    }

    private func runCfgSubscription(client: ProfilerClient, address: UInt64) async {
        while !Task.isCancelled {
            let params = ViewParams(
                tid: model_tidFilterAsU32(),
                filter: LiveFilter(timeRange: nil, excludeSymbols: [])
            )
            do {
                let update = try await client.cfg(address: address, params: params)
                NSLog(
                    "stax: cfg update for %@ (%d blocks, %d edges)",
                    update.functionName,
                    update.blocks.count,
                    update.edges.count
                )
                self.streamStats.cfg += 1
                self.cfg = update
            } catch {
                NSLog("stax: cfg poll failed: %@", "\(error)")
            }
            try? await Task.sleep(nanoseconds: 500_000_000)
        }
    }

    /// Cancel any in-flight neighbors subscription and start a new
    /// one for the currently-focused address (if any).
    private func restartNeighborsSubscription() {
        neighborsTask?.cancel()
        neighborsTask = nil
        neighbors = nil
        guard let address = focusedAddress else { return }
        guard case .ready(let client) = service.state else { return }

        neighborsTask = Task { [weak self] in
            await self?.runNeighborsSubscription(client: client, address: address)
        }
    }

    private func runNeighborsSubscription(
        client: ProfilerClient,
        address: UInt64
    ) async {
        while !Task.isCancelled {
            let params = ViewParams(
                tid: model_tidFilterAsU32(),
                filter: LiveFilter(timeRange: nil, excludeSymbols: [])
            )
            do {
                let update = try await client.neighbors(
                    address: address,
                    params: params
                )
                NSLog(
                    "stax: neighbors update (%d callers, %d callees)",
                    update.callersTree.children.count,
                    update.calleesTree.children.count
                )
                self.streamStats.neighbors += 1
                self.neighbors = update
                applyNeighbors(update)
            } catch {
                NSLog("stax: neighbors poll failed: %@", "\(error)")
            }
            try? await Task.sleep(nanoseconds: 500_000_000)
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
        while !Task.isCancelled {
            let params = ViewParams(
                tid: model_tidFilterAsU32(),
                filter: LiveFilter(timeRange: nil, excludeSymbols: [])
            )
            do {
                let update = try await client.annotated(
                    address: address,
                    params: params
                )
                NSLog(
                    "stax: annotated update for %@ (%d lines)",
                    update.functionName,
                    update.lines.count
                )
                self.streamStats.annotated += 1
                self.annotated = update
            } catch {
                NSLog("stax: annotated poll failed: %@", "\(error)")
            }
            try? await Task.sleep(nanoseconds: 500_000_000)
        }
    }
}

private func symbolKind(forLanguage language: String) -> SymbolKind {
    switch language.lowercased() {
    case "rust": .rust
    case "c": .c
    case "cpp", "c++": .cpp
    case "swift": .swift
    case "objc", "objective-c", "objectivec": .objc
    default: .unknown
    }
}
