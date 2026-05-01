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
                case .cfg:         CFGView(model: model)
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

// MARK: - CFG

private struct CFGView: View {
    @Bindable var model: AppModel

    var body: some View {
        Group {
            if let cfg = model.cfg, !cfg.blocks.isEmpty {
                CFGCanvas(cfg: cfg)
            } else if model.focusedAddress == nil {
                emptyState("Select a function to drill into.")
            } else {
                let address = model.focusedAddress ?? 0
                emptyState(
                    """
                    no CFG yet for 0x\(String(address, radix: 16))
                    (subscribed; the server may not have a binary for this address)
                    """
                )
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Color(nsColor: .textBackgroundColor).opacity(0.3))
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

private struct CFGCanvas: View {
    let cfg: CfgUpdate

    /// Pixel size of one column / one layer slot. Each block's actual
    /// height is computed from its line count; columns are uniform.
    private let columnWidth: CGFloat = 320
    private let columnGap: CGFloat = 24
    private let layerGap: CGFloat = 56
    private let lineHeight: CGFloat = 14
    private let blockHeader: CGFloat = 22
    private let blockPadding: CGFloat = 6
    /// Horizontal spacing between vertical tracks for right-routed
    /// edges. Each long-forward / back edge claims one track.
    private let trackSpacing: CGFloat = 12

    var body: some View {
        let layout = CFGLayout.layout(cfg)
        let placements = computePlacements(layout)
        let blocksWidth = max(
            columnWidth,
            CGFloat(layout.columns) * (columnWidth + columnGap) - columnGap
        )
        // Each long-forward / back edge gets its own vertical track on
        // the right of the block area. Without per-edge tracks two
        // edges with overlapping y-ranges would collapse onto one
        // line; with them, every edge has a distinct path.
        let edgeTracks = assignRightTracks(edges: layout.edges, placements: placements)
        let trackCount = (edgeTracks.values.max() ?? -1) + 1
        let trackStripWidth = trackCount > 0
            ? CGFloat(trackCount) * trackSpacing + columnGap
            : columnGap
        let canvasWidth = blocksWidth + trackStripWidth
        let canvasHeight = placements.totalHeight

        let maxCost = max(
            UInt64(1),
            cfg.blocks.flatMap(\.lines).map(\.selfOnCpuNs).max() ?? 0
        )

        ScrollView([.horizontal, .vertical]) {
            ZStack(alignment: .topLeading) {
                Canvas { ctx, _ in
                    for edge in layout.edges {
                        guard let from = placements.byId[edge.edge.fromId],
                              let to = placements.byId[edge.edge.toId]
                        else { continue }
                        drawEdge(
                            ctx: ctx,
                            from: from,
                            to: to,
                            edge: edge,
                            blocksWidth: blocksWidth,
                            trackIndex: edgeTracks[edge.id]
                        )
                    }
                }
                .frame(width: canvasWidth, height: canvasHeight)

                ForEach(layout.blocks) { laidOut in
                    if let placement = placements.byId[laidOut.id] {
                        BlockView(
                            block: laidOut.block,
                            queriedAddress: cfg.queriedAddress,
                            baseAddress: cfg.baseAddress,
                            maxCost: maxCost
                        )
                        .frame(width: columnWidth)
                        .position(
                            x: placement.center.x,
                            y: placement.center.y
                        )
                    }
                }
            }
            .frame(width: canvasWidth, height: canvasHeight)
            .padding(columnGap)
        }
    }

    private struct Placement {
        let frame: CGRect
        let center: CGPoint
        let layer: Int
    }

    private struct Placements {
        let byId: [UInt32: Placement]
        let totalHeight: CGFloat
    }

    /// Compute pixel rect per block. Within a layer all blocks share
    /// the same row height (the tallest block in that layer) so edges
    /// stay aligned.
    private func computePlacements(_ layout: LaidOutCFG) -> Placements {
        var placements: [UInt32: Placement] = [:]

        // Group blocks by layer.
        var byLayer: [Int: [LaidOutBlock]] = [:]
        for b in layout.blocks {
            byLayer[b.layer, default: []].append(b)
        }

        var y: CGFloat = 0
        for layer in 0..<layout.layers {
            let blocks = byLayer[layer] ?? []
            let rowHeight = blocks
                .map { blockHeight(linesCount: $0.block.lines.count) }
                .max() ?? 0

            for b in blocks {
                let blockH = blockHeight(linesCount: b.block.lines.count)
                let centerX = CGFloat(b.column) * (columnWidth + columnGap)
                    + columnWidth / 2
                let centerY = y + rowHeight / 2
                let frame = CGRect(
                    x: centerX - columnWidth / 2,
                    y: centerY - blockH / 2,
                    width: columnWidth,
                    height: blockH
                )
                placements[b.id] = Placement(
                    frame: frame,
                    center: CGPoint(x: centerX, y: centerY),
                    layer: b.layer
                )
            }

            y += rowHeight + layerGap
        }
        let total = max(0, y - layerGap)
        return Placements(byId: placements, totalHeight: total)
    }

    private func blockHeight(linesCount: Int) -> CGFloat {
        blockHeader + CGFloat(linesCount) * lineHeight + blockPadding * 2
    }

    private func drawEdge(
        ctx: GraphicsContext,
        from: Placement,
        to: Placement,
        edge: LaidOutEdge,
        blocksWidth: CGFloat,
        trackIndex: Int?
    ) {
        let goesRight = trackIndex != nil

        let start: CGPoint
        let end: CGPoint
        if goesRight {
            start = CGPoint(x: from.frame.maxX, y: from.center.y)
            end = CGPoint(x: to.frame.maxX, y: to.center.y)
        } else {
            start = CGPoint(x: from.center.x, y: from.frame.maxY)
            end = CGPoint(x: to.center.x, y: to.frame.minY)
        }

        var path = Path()
        if goesRight, let track = trackIndex {
            // Each right-routed edge runs in its own vertical track,
            // offset from the right edge of the block strip. Sized
            // and positioned so the canvas reserves room for it.
            let detour = blocksWidth + columnGap / 2 + CGFloat(track) * trackSpacing
            path.move(to: start)
            path.addLine(to: CGPoint(x: detour, y: start.y))
            path.addLine(to: CGPoint(x: detour, y: end.y))
            path.addLine(to: end)
        } else {
            // S-curve for adjacent-layer forward steps.
            let mid = (start.y + end.y) / 2
            path.move(to: start)
            path.addCurve(
                to: end,
                control1: CGPoint(x: start.x, y: mid),
                control2: CGPoint(x: end.x, y: mid)
            )
        }
        let stroke = StrokeStyle(
            lineWidth: 1,
            lineCap: .round,
            dash: edge.isBackEdge ? [4, 3] : []
        )
        ctx.stroke(path, with: .color(color(for: edge)), style: stroke)

        // Arrowhead.
        let approach: CGPoint = goesRight
            ? CGPoint(x: end.x + 12, y: end.y)  // entering from the right
            : CGPoint(x: end.x, y: end.y - 12)  // entering from above
        let head = arrowhead(at: end, comingFrom: approach)
        ctx.fill(head, with: .color(color(for: edge)))
    }

    /// Assign each right-routed edge to a vertical track on the
    /// right of the block strip. Edges are sorted by y-span and
    /// greedily packed into tracks so two edges that share any y
    /// range get distinct tracks. Result keys are `LaidOutEdge.id`.
    private func assignRightTracks(
        edges: [LaidOutEdge],
        placements: Placements
    ) -> [String: Int] {
        struct Candidate {
            let id: String
            let yLo: CGFloat
            let yHi: CGFloat
        }
        var candidates: [Candidate] = []
        for e in edges {
            guard let from = placements.byId[e.edge.fromId],
                  let to = placements.byId[e.edge.toId]
            else { continue }
            let layerSpan = to.layer - from.layer
            let goesRight = e.isBackEdge || layerSpan > 1
            guard goesRight else { continue }
            let yLo = min(from.center.y, to.center.y)
            let yHi = max(from.center.y, to.center.y)
            candidates.append(Candidate(id: e.id, yLo: yLo, yHi: yHi))
        }
        // Long edges first — they're harder to pack later.
        candidates.sort { ($0.yHi - $0.yLo) > ($1.yHi - $1.yLo) }

        var trackHi: [CGFloat] = []  // last assigned hi-y per track
        var trackLo: [CGFloat] = []  // first assigned lo-y per track
        var assigned: [String: Int] = [:]
        for c in candidates {
            // Find the lowest-numbered track whose existing range
            // doesn't intersect [c.yLo, c.yHi].
            var picked = -1
            for i in 0..<trackHi.count {
                if c.yHi <= trackLo[i] || c.yLo >= trackHi[i] {
                    trackLo[i] = min(trackLo[i], c.yLo)
                    trackHi[i] = max(trackHi[i], c.yHi)
                    picked = i
                    break
                }
            }
            if picked < 0 {
                picked = trackHi.count
                trackHi.append(c.yHi)
                trackLo.append(c.yLo)
            }
            assigned[c.id] = picked
        }
        return assigned
    }

    private func arrowhead(at tip: CGPoint, comingFrom: CGPoint) -> Path {
        let dx = tip.x - comingFrom.x
        let dy = tip.y - comingFrom.y
        let len = max(0.001, (dx * dx + dy * dy).squareRoot())
        let ux = dx / len
        let uy = dy / len
        let size: CGFloat = 6
        let leftX = tip.x - ux * size + uy * size * 0.5
        let leftY = tip.y - uy * size - ux * size * 0.5
        let rightX = tip.x - ux * size - uy * size * 0.5
        let rightY = tip.y - uy * size + ux * size * 0.5
        var path = Path()
        path.move(to: tip)
        path.addLine(to: CGPoint(x: leftX, y: leftY))
        path.addLine(to: CGPoint(x: rightX, y: rightY))
        path.closeSubpath()
        return path
    }

    private func color(for edge: LaidOutEdge) -> Color {
        switch edge.edge.kind {
        case .fallthrough:        return .secondary.opacity(0.55)
        case .branch:             return Color(red: 0.537, green: 0.706, blue: 0.980).opacity(0.85)
        case .conditionalBranch:  return Color(red: 0.976, green: 0.886, blue: 0.686).opacity(0.85)
        case .call:               return Color(red: 0.580, green: 0.886, blue: 0.835).opacity(0.85)
        }
    }
}

private struct BlockView: View {
    let block: BasicBlock
    let queriedAddress: UInt64
    let baseAddress: UInt64
    let maxCost: UInt64

    var body: some View {
        let containsQueried = block.lines.contains { $0.address == queriedAddress }
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 6) {
                Text(addressOffset(block.startAddress, base: baseAddress))
                    .font(.mono(.caption2))
                    .foregroundStyle(.secondary)
                Spacer()
                Text("\(block.lines.count) instr")
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
            }
            .padding(.horizontal, 6)
            .padding(.vertical, 4)
            .background(Color(nsColor: .underPageBackgroundColor))

            ForEach(Array(block.lines.enumerated()), id: \.offset) { _, line in
                HStack(spacing: 6) {
                    BarTrack(ratio: Double(line.selfOnCpuNs) / Double(maxCost))
                        .frame(width: 22, height: 2)
                    Text(addressOffset(line.address, base: baseAddress))
                        .foregroundStyle(.tertiary)
                        .frame(width: 56, alignment: .trailing)
                    tokenLine(line.tokens, fallback: .primary)
                        .lineLimit(1)
                }
                .font(.mono(.caption2))
                .padding(.horizontal, 6)
            }
        }
        .padding(.vertical, 4)
        .background(
            RoundedRectangle(cornerRadius: 6)
                .fill(Color(nsColor: .textBackgroundColor).opacity(0.9))
        )
        .overlay(
            RoundedRectangle(cornerRadius: 6)
                .stroke(
                    containsQueried
                        ? Color.accentColor
                        : Color.secondary.opacity(0.35),
                    lineWidth: containsQueried ? 1.5 : 0.75
                )
        )
    }
}
