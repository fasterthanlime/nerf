import Foundation
import VoxRuntime

/// Owns the live vox session to stax-server and exposes a
/// `ProfilerClient`. Single instance per AppModel.
@MainActor
final class ProfilerService {
    enum State {
        case idle
        case connecting
        case ready(ProfilerClient)
        case failed(String)
    }

    private(set) var state: State = .idle
    private var session: Session?
    private var driverTask: Task<Void, Error>?

    /// Connect to stax-server. Idempotent.
    func connect() async {
        if case .ready = state { return }
        state = .connecting

        guard let path = Self.resolveSocketPath() else {
            state = .failed("no stax-server socket found (set STAX_SERVER_SOCKET)")
            return
        }

        do {
            let connector = UnixConnector(path: path)
            let session = try await Session.initiator(
                connector,
                expecting: ProfilerClient.self,
                dispatcher: NoopDispatcher(),
                resumable: false
            )
            self.session = session
            self.driverTask = Task.detached { try await session.run() }
            self.state = .ready(ProfilerClient(connection: session.connection))
        } catch {
            state = .failed("connect failed: \(error)")
        }
    }

    func shutdown() {
        driverTask?.cancel()
        session?.handle.shutdown()
        session = nil
        driverTask = nil
        state = .idle
    }

    /// stax-server places its unix socket at
    /// `<app-group-container>/stax-server.sock` so a sandboxed peer
    /// (us) can reach it via the `application-groups` entitlement.
    private static let appGroup = "B2N6FSRTPV.eu.bearcove.stax"
    private static let socketName = "stax-server.sock"

    private static func resolveSocketPath() -> String? {
        let fm = FileManager.default
        if let env = ProcessInfo.processInfo.environment["STAX_SERVER_SOCKET"],
            fm.fileExists(atPath: env)
        {
            return env
        }
        if let containerURL = fm.containerURL(forSecurityApplicationGroupIdentifier: appGroup) {
            let p = containerURL.appendingPathComponent(socketName).path
            if fm.fileExists(atPath: p) { return p }
        }
        return nil
    }
}

/// Stub dispatcher. We're a pure client; any incoming method ID gets
/// the standard "unknown method" reply.
private struct NoopDispatcher: ServiceDispatcher {
    func dispatch(
        methodId: UInt64,
        payload: [UInt8],
        requestId: UInt64,
        registry: ChannelRegistry,
        schemaSendTracker: SchemaSendTracker,
        taskTx: @escaping @Sendable (TaskMessage) -> Void
    ) async {
        taskTx(.response(requestId: requestId, payload: encodeUnknownMethodError()))
    }

    func preregister(methodId: UInt64, payload: [UInt8], registry: ChannelRegistry) async {}
}
