import SwiftUI

struct StatusBar: View {
    @Bindable var model: AppModel

    var body: some View {
        HStack {
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
