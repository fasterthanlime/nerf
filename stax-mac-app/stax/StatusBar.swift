import SwiftUI

struct StatusBar: View {
    @Bindable var model: AppModel

    var body: some View {
        HStack(spacing: 8) {
            Circle()
                .fill(connectionColor)
                .frame(width: 7, height: 7)
            Text(model.connectionStatus)
                .font(.caption)
                .foregroundStyle(.secondary)
                .lineLimit(1)

            Text(streamCountersText)
                .font(.mono(.caption2))
                .foregroundStyle(.tertiary)
                .lineLimit(1)

            Spacer()
            Text(statsText)
                .font(.mono(.callout))
                .foregroundStyle(.secondary)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
        .background(.bar)
        .overlay(alignment: .top) { Divider() }
    }

    /// Compact line of `stream:count` per active subscription. Lets
    /// you see at a glance whether each stream has produced anything,
    /// without grepping the unified log. Zeros mean "subscribed but
    /// no update yet" — server has no data, hasn't ticked, or the
    /// connection didn't actually route to that service.
    private var streamCountersText: String {
        let s = model.streamStats
        return "thr:\(s.threads) top:\(s.top) flame:\(s.flamegraph) tl:\(s.timeline) nbr:\(s.neighbors) ann:\(s.annotated)"
    }

    private var connectionColor: Color {
        switch model.connectionStatus {
        case "connected": .green
        case "connecting": .orange
        default: .red
        }
    }

    private var statsText: String {
        let onCPU = formatDuration(model.onCPUTime)
        let offCPU = formatDuration(model.offCPUTime)
        return "\(onCPU) on-CPU · \(offCPU) off-CPU · \(model.symbolCount) symbols"
    }
}

func formatDuration(_ seconds: TimeInterval) -> String {
    let abs = Swift.abs(seconds)
    if abs == 0 { return "0" }
    if abs < 1e-6 { return String(format: "%.0fns", seconds * 1_000_000_000) }
    if abs < 1e-3 { return String(format: "%.1f\u{00B5}s", seconds * 1_000_000) }
    if abs < 1    { return String(format: "%.1fms", seconds * 1_000) }
    return String(format: "%.2fs", seconds)
}
