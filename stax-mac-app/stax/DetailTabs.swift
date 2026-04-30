import SwiftUI

struct DetailTabs: View {
    @Bindable var model: AppModel
    @State private var tab: Tab = .disassembly

    enum Tab: String, CaseIterable, Identifiable {
        case disassembly = "disassembly"
        case cfg         = "cfg"
        case intervals   = "intervals"
        var id: String { rawValue }
    }

    var body: some View {
        VStack(spacing: 0) {
            Picker("", selection: $tab) {
                ForEach(Tab.allCases) { t in
                    Text(t.rawValue).tag(t)
                }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .padding(8)

            Divider()

            ZStack {
                switch tab {
                case .disassembly: DisassemblyView(model: model)
                case .cfg:         CFGView()
                case .intervals:   IntervalsView(model: model)
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }
}

// MARK: - Disassembly

private struct DisassemblyView: View {
    @Bindable var model: AppModel

    var body: some View {
        Group {
            if let view = model.annotated {
                liveBody(view)
            } else if model.focusedAddress == nil {
                emptyState("Select a function to drill into.")
            } else {
                // Stream is subscribed but no update has arrived. Common
                // when the address is in an image stax-server hasn't
                // resolved (no DWARF / not yet seen / dlclosed) — the
                // server simply doesn't emit anything. Tell the user
                // why this might be quiet instead of pretending it's
                // about to load.
                let address = model.focusedAddress ?? 0
                emptyState(
                    """
                    no disassembly yet for 0x\(String(address, radix: 16))
                    (subscribed; the server may not have a binary for this address)
                    """
                )
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Color(nsColor: .textBackgroundColor).opacity(0.3))
    }

    @ViewBuilder
    private func liveBody(_ view: AnnotatedView) -> some View {
        let maxCost = max(
            1,
            view.lines.map(\.selfOnCpuNs).max() ?? 0
        )
        ScrollView {
            VStack(alignment: .leading, spacing: 0) {
                ForEach(Array(view.lines.enumerated()), id: \.offset) { _, line in
                    AnnotatedLineRow(line: line, baseAddress: view.baseAddress, maxCost: maxCost)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 12)
            .padding(.vertical, 8)
        }
    }

    private func emptyState(_ text: String) -> some View {
        VStack {
            Text(text)
                .font(.mono(.callout))
                .foregroundStyle(.tertiary)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

private struct AnnotatedLineRow: View {
    let line: AnnotatedLine
    let baseAddress: UInt64
    let maxCost: UInt64

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            if let header = line.sourceHeader {
                Text("\(header.file):\(header.line)")
                    .font(.mono(.caption))
                    .foregroundStyle(.secondary)
                    .padding(.top, 4)
                tokenLine(header.tokens, fallback: .tertiary)
                    .font(.mono(.caption))
                    .padding(.bottom, 2)
            }
            HStack(spacing: 8) {
                BarTrack(ratio: Double(line.selfOnCpuNs) / Double(maxCost))
                    .frame(width: 36, height: 3)
                Text(percentLabel(Double(line.selfOnCpuNs) / Double(maxCost)))
                    .foregroundStyle(.tertiary)
                    .frame(width: 32, alignment: .trailing)
                Text(addressOffset(line.address, base: baseAddress))
                    .foregroundStyle(.tertiary)
                    .frame(width: 64, alignment: .trailing)
                tokenLine(line.tokens, fallback: .primary)
                    .lineLimit(1)
            }
            .font(.mono(.caption))
        }
    }
}

private func addressOffset(_ addr: UInt64, base: UInt64) -> String {
    if addr >= base {
        return String(format: "+0x%llx", addr - base)
    }
    return String(format: "0x%llx", addr)
}

/// Render a sequence of classified tokens as one concatenated `Text`.
/// Adjacent tokens are joined with `+` so SwiftUI keeps them on one
/// line and only paints colour where the server tagged something.
/// Anything classified as `.plain` falls back to `fallback`.
@ViewBuilder
private func tokenLine(_ tokens: [Token], fallback: HierarchicalShapeStyle) -> some View {
    if tokens.isEmpty {
        Text("")
    } else {
        tokens
            .map { tok in
                if let color = tokenColor(tok.kind) {
                    return Text(tok.text).foregroundStyle(color)
                } else {
                    return Text(tok.text).foregroundStyle(fallback)
                }
            }
            .reduce(Text(""), +)
    }
}

/// Catppuccin Mocha palette, lifted from arborium's default theme so
/// the app reads roughly like the web frontend used to. `nil` means
/// "use the caller's fallback colour" — applied to `.plain` and to
/// classes the theme doesn't paint (e.g. `.literal`).
private func tokenColor(_ kind: TokenClass) -> Color? {
    switch kind {
    case .plain:         return nil
    case .keyword:       return Color(red: 0.796, green: 0.651, blue: 0.969) // #cba6f7
    case .function:      return Color(red: 0.537, green: 0.706, blue: 0.980) // #89b4fa
    case .string:        return Color(red: 0.651, green: 0.890, blue: 0.631) // #a6e3a1
    case .comment:       return Color(red: 0.424, green: 0.439, blue: 0.525) // #6c7086
    case .type:          return Color(red: 0.976, green: 0.886, blue: 0.686) // #f9e2af
    case .variable:      return nil
    case .constant:      return Color(red: 0.980, green: 0.702, blue: 0.529) // #fab387
    case .number:        return Color(red: 0.980, green: 0.702, blue: 0.529) // #fab387
    case .operator:      return Color(red: 0.580, green: 0.886, blue: 0.835) // #94e2d5
    case .punctuation:   return Color(red: 0.576, green: 0.604, blue: 0.698) // #9399b2
    case .property:      return Color(red: 0.537, green: 0.706, blue: 0.980) // #89b4fa
    case .attribute:     return Color(red: 0.976, green: 0.886, blue: 0.686) // #f9e2af
    case .tag:           return Color(red: 0.537, green: 0.706, blue: 0.980) // #89b4fa
    case .macro:         return Color(red: 0.580, green: 0.886, blue: 0.835) // #94e2d5
    case .label:         return Color(red: 0.961, green: 0.761, blue: 0.906) // #f5c2e7
    case .namespace:     return nil
    case .constructor:   return nil
    case .title:         return Color(red: 0.796, green: 0.651, blue: 0.969) // #cba6f7
    case .strong:        return nil
    case .emphasis:      return nil
    case .link:          return Color(red: 0.537, green: 0.706, blue: 0.980) // #89b4fa
    case .literal:       return nil
    case .strikethrough: return nil
    case .diffAdd:       return Color(red: 0.651, green: 0.890, blue: 0.631) // #a6e3a1
    case .diffDelete:    return Color(red: 0.953, green: 0.545, blue: 0.659) // #f38ba8
    case .embedded:      return nil
    case .error:         return Color(red: 0.953, green: 0.545, blue: 0.659) // #f38ba8
    }
}

private func percentLabel(_ ratio: Double) -> String {
    if ratio < 0.005 { return "" }
    return String(format: "%.1f%%", ratio * 100)
}

// MARK: - Intervals

private struct IntervalsView: View {
    @Bindable var model: AppModel
    @State private var selection: AppModel.Interval.ID?

    var body: some View {
        Group {
            if model.intervals.isEmpty {
                VStack(spacing: 6) {
                    Text("intervals not wired yet")
                        .font(.mono(.callout))
                        .foregroundStyle(.tertiary)
                    Text("needs a flame-graph stack-frame click → flame_key")
                        .font(.caption)
                        .foregroundStyle(.tertiary)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                liveBody
            }
        }
        .background(Color(nsColor: .textBackgroundColor).opacity(0.3))
    }

    private var liveBody: some View {
        VStack(spacing: 0) {
            HStack {
                Text(
                    "\(model.intervalsTotalCount.formatted()) intervals · \(formatDuration(model.intervalsTotalDuration)) total"
                )
                .font(.mono(.caption))
                .foregroundStyle(.secondary)
                Spacer()
            }
            .padding(.horizontal, 12)
            .padding(.top, 8)
            .padding(.bottom, 6)

            Divider()

            Table(model.intervals, selection: $selection) {
                TableColumn("START") { i in
                    Text(String(format: "%.3fs", i.start))
                        .font(.mono(.caption))
                        .foregroundStyle(.secondary)
                }
                .width(min: 50, ideal: 60, max: 80)

                TableColumn("DURATION") { i in
                    Text(formatDuration(i.duration))
                        .font(.mono(.caption))
                        .frame(maxWidth: .infinity, alignment: .trailing)
                }
                .width(min: 60, ideal: 80, max: 110)

                TableColumn("REASON") { i in
                    Text(i.reason.rawValue)
                        .font(.mono(.caption))
                        .foregroundStyle(i.reason.color)
                }
                .width(min: 50, ideal: 70, max: 90)

                TableColumn("TID") { i in
                    Text(String(i.tid))
                        .font(.mono(.caption))
                        .foregroundStyle(.secondary)
                }
                .width(min: 60, ideal: 80, max: 100)

                TableColumn("WOKEN BY") { i in
                    Text(i.wokenBy.map { String($0) } ?? "(none)")
                        .font(.mono(.caption))
                        .foregroundStyle(.tertiary)
                }
            }
        }
    }
}

// MARK: - CFG (placeholder)

private struct CFGView: View {
    var body: some View {
        VStack(spacing: 8) {
            Text("control flow graph not wired yet")
                .font(.mono(.callout))
                .foregroundStyle(.tertiary)
            Text("needs a server-side subscribe_cfg(address)")
                .font(.caption)
                .foregroundStyle(.tertiary)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Color(nsColor: .textBackgroundColor).opacity(0.3))
    }
}
