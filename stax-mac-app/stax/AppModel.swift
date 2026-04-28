import Foundation
import Observation
import SwiftUI

@Observable
@MainActor
final class AppModel {
    var paused: Bool = false
    /// `nil` means all threads.
    var threadFilter: Int? = nil
    /// Quoted (`"foo"`) → exact substring. Slashed (`/foo/`) → regex.
    /// Plain text → fuzzy substring.
    var searchQuery: String = ""

    /// `nil` → top pane shows the flame graph. Non-nil → top pane shows the
    /// call graph centered on the focused function.
    var focusedFunctionId: FunctionEntry.ID? = nil

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
    var threads: [ThreadInfo] = [
        .init(tid: 1,    name: "main",     onCPU: 2.40),
        .init(tid: 1024, name: "worker-1", onCPU: 0.36),
        .init(tid: 1025, name: "worker-2", onCPU: 0.24),
        .init(tid: 1026, name: "worker-3", onCPU: 0.06),
    ]

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

    var functions: [FunctionEntry] = [
        .init(name: "start_wqthread",                                              binary: "libsystem_pthread.dylib", kind: .c,     selfTime: 0.0012,    totalTime: 0.0024),
        .init(name: "core::str::converts::from_utf8",                              binary: "transcribe-metal",        kind: .rust,  selfTime: 0,         totalTime: 0),
        .init(name: "__psynch_cvwait",                                             binary: "libsystem_kernel.dylib",  kind: .c,     selfTime: 0.0000766, totalTime: 0.0000766),
        .init(name: "write",                                                       binary: "libsystem_kernel.dylib",  kind: .c,     selfTime: 0,         totalTime: 0),
        .init(name: "iokit_user_client_trap",                                      binary: "IOKit",                   kind: .c,     selfTime: 0.0012,    totalTime: 0.0012),
        .init(name: "rustfft::algorithm::mixed_radix",                             binary: "transcribe-metal",        kind: .rust,  selfTime: 0,         totalTime: 0),
        .init(name: "core::hash::BuildHasher::hash_one",                           binary: "transcribe-metal",        kind: .rust,  selfTime: 0,         totalTime: 0),
        .init(name: "-[_MTLCommandQueue _submitAvailableCommandBuffers]",          binary: "Metal",                   kind: .objc,  selfTime: 0,         totalTime: 0),
        .init(name: "-[IOGPUMetalCommandQueue submitCommandBuffers:count:]",       binary: "IOGPU",                   kind: .objc,  selfTime: 0,         totalTime: 0),
        .init(name: "0x1010728c8",                                                 binary: "(no binary)",             kind: .unknown, selfTime: 0.000991, totalTime: 0.000994),
    ]
}
