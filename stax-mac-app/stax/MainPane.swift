import SwiftUI

struct MainPane: View {
    @Bindable var model: AppModel

    var body: some View {
        VSplitView {
            topPane
                .frame(minHeight: 200)

            HSplitView {
                FunctionTable(model: model)
                    .frame(minWidth: 320)
                DetailTabs(model: model)
                    .frame(minWidth: 320)
            }
            .frame(minHeight: 160)
        }
    }

    @ViewBuilder
    private var topPane: some View {
        if let display = model.focusedDisplay {
            VStack(spacing: 0) {
                NavHeader(model: model, focused: display)
                Divider()
                CallGraphView(model: model)
            }
        } else {
            VStack(spacing: 0) {
                minimap
                Divider()
                flame
            }
        }
    }

    private var minimap: some View {
        Minimap(timeline: model.timeline)
            .frame(height: 56)
    }

    private var flame: some View {
        FlameView(
            flamegraph: model.flamegraph,
            onOpen: { node in
                guard let fg = model.flamegraph else { return }
                model.focusOnFlameNode(node, strings: fg.strings)
            }
        )
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

// MARK: - Flame View

/// Stacked-tree flame graph with highlight-on-single-click,
/// double-click to drill into call graph, vertical scrolling, and
/// keyboard shortcuts (Enter to open, Esc to clear highlight).
private struct FlameView: View {
    let flamegraph: FlamegraphUpdate?
    var onOpen: (FlameNode) -> Void = { _ in }

    @State private var highlightedAddress: UInt64? = nil

    var body: some View {
        VStack(spacing: 0) {
            ZStack {
                Color(nsColor: .textBackgroundColor).opacity(0.3)
                if let flamegraph {
                    GeometryReader { geo in
                        let cells = layoutFlame(
                            root: flamegraph.root, in: geo.size)
                        let totalHeight = flameTotalHeight(cells: cells)
                        ScrollView(.vertical) {
                            ZStack(alignment: .topLeading) {
                                Canvas { ctx, _ in
                                    for cell in cells {
                                        drawFlameCell(
                                            cell,
                                            highlighted: cell.node
                                                .address
                                                == highlightedAddress,
                                            in: ctx,
                                            strings: flamegraph
                                                .strings)
                                    }
                                }
                                .frame(
                                    width: geo.size.width,
                                    height: totalHeight)

                                // Per-cell clear tap targets for
                                // double-tap detection. SwiftUI
                                // doesn't support simultaneously
                                // recognising single + double tap on
                                // the same view, so we overlay
                                // invisible rects.
                                ForEach(
                                    cells, id: \.rect.debugDescription
                                ) { cell in
                                    Color.clear
                                        .contentShape(.rect)
                                        .frame(
                                            width: cell.rect.width,
                                            height: cell.rect.height
                                        )
                                        .position(
                                            x: cell.rect.midX,
                                            y: cell.rect.midY
                                        )
                                        .gesture(
                                            TapGesture(count: 2)
                                                .onEnded {
                                                    onOpen(cell.node)
                                                }
                                                .exclusively(
                                                    before:
                                                        SpatialTapGesture()
                                                        .onEnded {
                                                            _ in
                                                            highlightedAddress =
                                                                cell
                                                                .node
                                                                .address
                                                        }))
                                }
                            }
                            .contentShape(.rect)
                            .onTapGesture { _ in
                                highlightedAddress = nil
                            }
                        }
                    }
                } else {
                    Text("waiting for flamegraph…")
                        .font(.mono(.callout))
                        .foregroundStyle(.tertiary)
                }
            }
            flameStatusBar
        }
        .focusable()
        .focusEffectDisabled()
        .onKeyPress(.return) {
            guard
                let addr = highlightedAddress,
                let fg = flamegraph,
                addr != 0,
                let node = findNode(address: addr, in: fg.root)
            else { return .ignored }
            onOpen(node)
            return .handled
        }
        .onKeyPress(.escape) {
            highlightedAddress = nil
            return .handled
        }
    }

    private var flameStatusBar: some View {
        HStack(spacing: 8) {
            if let addr = highlightedAddress,
                let fg = flamegraph,
                let node = findNode(address: addr, in: fg.root)
            {
                let name =
                    stringAt(node.functionName, in: fg.strings)
                    ?? hex(node.address)
                let bin =
                    stringAt(node.binary, in: fg.strings) ?? ""
                let onCPU = formatDuration(
                    TimeInterval(node.onCpuNs) / 1_000_000_000)
                let pct =
                    fg.totalOnCpuNs > 0
                    ? String(
                        format: "%.1f%%",
                        Double(node.onCpuNs)
                            / Double(fg.totalOnCpuNs) * 100)
                    : "0%"
                Text(name)
                    .font(.mono(.caption))
                    .foregroundStyle(.primary)
                    .lineLimit(1)
                Text(bin)
                    .font(.mono(.caption2))
                    .foregroundStyle(.tertiary)
                    .lineLimit(1)
                Spacer()
                Text("\(onCPU) · \(pct)")
                    .font(.mono(.caption2))
                    .foregroundStyle(.secondary)
            } else if flamegraph != nil {
                Text("click to highlight · double-click to open")
                    .font(.mono(.caption2))
                    .foregroundStyle(.tertiary)
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 4)
        .background(.bar)
    }
}

private func findNode(address: UInt64, in root: FlameNode) -> FlameNode? {
    if root.address == address { return root }
    for child in root.children {
        if let found = findNode(address: address, in: child) {
            return found
        }
    }
    return nil
}

private struct LaidOutFlameCell {
    let node: FlameNode
    let rect: CGRect
}

private let flameLevelHeight: CGFloat = 18
private let flameMaxDepth = 32

/// One-pass tidy-tree layout: each level is a fixed-height row,
/// children fill their parent's width proportional to `on_cpu_ns`.
/// Returns cells in pre-order (root first) so the renderer paints
/// shallow before deep, and so reverse-walk hit-testing finds the
/// deepest covering cell first.
private func layoutFlame(root: FlameNode, in size: CGSize) -> [LaidOutFlameCell] {
    var cells: [LaidOutFlameCell] = []
    func walk(_ node: FlameNode, rect: CGRect, depth: Int) {
        if depth >= flameMaxDepth { return }
        if rect.width < 0.5 { return }
        cells.append(LaidOutFlameCell(node: node, rect: rect))

        let parentNs = max(1, node.onCpuNs)
        var childX = rect.minX
        for child in node.children {
            let w = rect.width * CGFloat(child.onCpuNs) / CGFloat(parentNs)
            if w >= 0.5 {
                walk(
                    child,
                    rect: CGRect(
                        x: childX,
                        y: CGFloat(depth + 1) * flameLevelHeight,
                        width: w,
                        height: flameLevelHeight - 1),
                    depth: depth + 1)
            }
            childX += w
        }
    }
    walk(
        root,
        rect: CGRect(x: 0, y: 0, width: size.width, height: flameLevelHeight - 1),
        depth: 0)
    return cells
}

private func flameTotalHeight(cells: [LaidOutFlameCell]) -> CGFloat {
    cells.map(\.rect.maxY).max() ?? 0
}

private func drawFlameCell(
    _ cell: LaidOutFlameCell,
    highlighted: Bool,
    in ctx: GraphicsContext,
    strings: [String]
) {
    let fill = flameNodeColor(cell.node, strings: strings)
    ctx.fill(Path(cell.rect), with: .color(fill))

    if highlighted {
        ctx.stroke(
            Path(cell.rect),
            with: .color(.white.opacity(0.9)),
            lineWidth: 1.5)
    }

    if cell.rect.width >= 32 {
        let name = stringAt(cell.node.functionName, in: strings) ?? hex(cell.node.address)
        let text = Text(name)
            .font(.mono(.caption2))
            .foregroundStyle(.primary)
        ctx.draw(text, in: cell.rect.insetBy(dx: 4, dy: 1))
    }
}

private func stringAt(_ idx: UInt32?, in strings: [String]) -> String? {
    guard let i = idx, Int(i) < strings.count else { return nil }
    return strings[Int(i)]
}

private func hex(_ addr: UInt64) -> String {
    String(format: "0x%llx", addr)
}

private func flameNodeColor(_ node: FlameNode, strings: [String]) -> Color {
    let lang = (Int(node.language) < strings.count ? strings[Int(node.language)] : "")
        .lowercased()
    switch lang {
    case "rust": return Color(red: 0.74, green: 0.56, blue: 0.91).opacity(0.6)
    case "c": return Color(red: 0.36, green: 0.78, blue: 0.85).opacity(0.6)
    case "cpp", "c++": return Color(red: 0.36, green: 0.65, blue: 0.95).opacity(0.6)
    case "swift": return Color(red: 0.95, green: 0.55, blue: 0.43).opacity(0.6)
    case "objc",
        "objective-c",
        "objectivec":
        return Color(red: 0.55, green: 0.82, blue: 0.45).opacity(0.6)
    default: return Color(red: 0.96, green: 0.78, blue: 0.27).opacity(0.6)
    }
}

private struct Minimap: View {
    let timeline: TimelineUpdate?

    var body: some View {
        ZStack {
            Color(nsColor: .underPageBackgroundColor)
            if let timeline, !timeline.buckets.isEmpty {
                Canvas { ctx, size in
                    drawTimeline(timeline, in: ctx, size: size)
                }
            } else {
                Text("waiting for timeline…")
                    .font(.caption)
                    .foregroundStyle(.tertiary)
            }
        }
    }

    /// Each bucket renders as two stacked columns: on-CPU (green) at
    /// the bottom, off-CPU (gray) above it. Bar height is fraction of
    /// the bucket-window the thread group spent in that state.
    private func drawTimeline(
        _ timeline: TimelineUpdate,
        in ctx: GraphicsContext,
        size: CGSize
    ) {
        guard !timeline.buckets.isEmpty else { return }
        let bucketCount = timeline.buckets.count
        let bucketWidth = size.width / CGFloat(bucketCount)
        let bucketSizeNs = max(1, Double(timeline.bucketSizeNs))

        // Cap at the bucket size — saturated buckets reach the top.
        let onColor = Color.green.opacity(0.7)
        let offColor = Color.gray.opacity(0.45)

        for (i, bucket) in timeline.buckets.enumerated() {
            let x = CGFloat(i) * bucketWidth
            let onRatio = min(1, Double(bucket.onCpuNs) / bucketSizeNs)
            let offRatio = min(1, Double(bucket.offCpuNs) / bucketSizeNs)

            let onHeight = CGFloat(onRatio) * size.height
            let offHeight = CGFloat(offRatio) * (size.height - onHeight)

            let onRect = CGRect(
                x: x,
                y: size.height - onHeight,
                width: max(1, bucketWidth - 0.5),
                height: onHeight
            )
            let offRect = CGRect(
                x: x,
                y: size.height - onHeight - offHeight,
                width: max(1, bucketWidth - 0.5),
                height: offHeight
            )
            ctx.fill(Path(onRect), with: .color(onColor))
            ctx.fill(Path(offRect), with: .color(offColor))
        }
    }
}

private struct NavHeader: View {
    @Bindable var model: AppModel
    let focused: AppModel.FocusedDisplay

    var body: some View {
        HStack(spacing: 8) {
            Button {
                model.clearFocus()
            } label: {
                HStack(spacing: 3) {
                    Image(systemName: "chevron.left")
                    Text("flame")
                }
                .font(.caption)
            }
            .buttonStyle(.plain)
            .help("Back to flame graph")

            Image(systemName: "chevron.right")
                .font(.caption2)
                .foregroundStyle(.tertiary)

            LanguageBadge(kind: focused.kind, size: 12)
            Text(focused.name)
                .font(.mono(.caption))
                .lineLimit(1)
            Text(focused.binary)
                .font(.mono(.caption))
                .foregroundStyle(.tertiary)
                .lineLimit(1)

            Spacer()
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
        .background(.bar)
    }
}
