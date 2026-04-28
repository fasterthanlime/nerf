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
                case .disassembly: DisassemblyPlaceholder()
                case .cfg:         CFGView()
                case .intervals:   IntervalsView(model: model)
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }
}

// MARK: - Disassembly

private struct AsmLine: Hashable {
    let addr: String
    let op: String
    let args: String
    let cost: Double  // 0...1
}

private struct DisassemblyPlaceholder: View {
    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 0) {
                AsmRow(label: "validations.rs:133", lines: [
                    .init(addr: "+0x0",  op: "subs",  args: "x9, x1, #0xf",                cost: 0.04),
                    .init(addr: "+0x4",  op: "csel",  args: "x10, xzr, x9, lo",            cost: 0.02),
                ])
                AsmRow(label: "validations.rs:145", lines: [
                    .init(addr: "+0x8",  op: "cbz",   args: "x1, $+0x204",                  cost: 0.18),
                    .init(addr: "+0xc",  op: "mov",   args: "x9, #0x0",                     cost: 0.01),
                    .init(addr: "+0x10", op: "add",   args: "x11, x0, #0x7",                cost: 0.62),
                    .init(addr: "+0x14", op: "and",   args: "x11, x11, #0xfffffffffffffff8",cost: 0.85),
                    .init(addr: "+0x18", op: "sub",   args: "x11, x11, x0",                 cost: 0.10),
                    .init(addr: "+0x1c", op: "adrp",  args: "x12, $+0x292000",              cost: 0.07),
                    .init(addr: "+0x20", op: "add",   args: "x12, x12, #0x18d",             cost: 0.05),
                    .init(addr: "+0x24", op: "b",     args: "$+0x10",                       cost: 0.03),
                ])
                AsmRow(label: "validations.rs:217", lines: [
                    .init(addr: "+0x28", op: "add",   args: "x9, x13, #0x1",                cost: 0.08),
                ])
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 12)
            .padding(.vertical, 8)
        }
        .background(Color(nsColor: .textBackgroundColor).opacity(0.3))
    }
}

private struct AsmRow: View {
    let label: String
    let lines: [AsmLine]

    var body: some View {
        VStack(alignment: .leading, spacing: 1) {
            Text(label)
                .font(.mono(.caption))
                .foregroundStyle(.secondary)
                .padding(.top, 4)
            Text("(source not on disk)")
                .font(.mono(.caption))
                .foregroundStyle(.tertiary)
                .padding(.bottom, 4)
            ForEach(lines, id: \.addr) { line in
                HStack(spacing: 8) {
                    BarTrack(ratio: line.cost)
                        .frame(width: 36, height: 3)
                    Text(percentLabel(line.cost))
                        .foregroundStyle(.tertiary)
                        .frame(width: 32, alignment: .trailing)
                    Text(line.addr)
                        .foregroundStyle(.tertiary)
                        .frame(width: 50, alignment: .trailing)
                    Text(line.op)
                        .foregroundStyle(.primary)
                        .frame(width: 50, alignment: .leading)
                    Text(line.args)
                        .foregroundStyle(.secondary)
                }
                .font(.mono(.caption))
            }
        }
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
        VStack(spacing: 0) {
            HStack {
                Text("\(model.intervalsTotalCount.formatted()) intervals · \(formatDuration(model.intervalsTotalDuration)) total")
                    .font(.mono(.caption))
                    .foregroundStyle(.secondary)
                Spacer()
                Text("showing 10000 most recent")
                    .font(.caption)
                    .foregroundStyle(.tertiary)
            }
            .padding(.horizontal, 12)
            .padding(.top, 8)
            .padding(.bottom, 6)

            ReasonPills()
                .padding(.horizontal, 12)
                .padding(.bottom, 8)

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

private struct ReasonPills: View {
    var body: some View {
        HStack(spacing: 6) {
            ForEach(AppModel.IntervalReason.allCases) { r in
                HStack(spacing: 5) {
                    Text(r.rawValue)
                    Text(formatDuration(r.fakeStat))
                        .foregroundStyle(.secondary)
                }
                .font(.mono(.caption))
                .padding(.horizontal, 6)
                .padding(.vertical, 2)
                .background(r.color.opacity(0.18), in: .rect(cornerRadius: 3))
                .overlay {
                    RoundedRectangle(cornerRadius: 3)
                        .stroke(r.color.opacity(0.5), lineWidth: 0.5)
                }
            }
        }
    }
}

// MARK: - CFG (control flow graph with inline assembly)

private struct CFGInstruction: Hashable {
    let addr: String
    let mnemonic: String
    let operands: String
}

private struct CFGNode: Identifiable {
    let id: String
    let label: String
    let instructions: [CFGInstruction]
    let rect: CGRect
    let hot: Bool
}

private enum CFGEdgeKind { case fallThrough, branch, backEdge }
private struct CFGEdge { let from: String; let to: String; let kind: CFGEdgeKind }

private struct CFGView: View {
    private let nodes: [CFGNode] = [
        CFGNode(
            id: "bb0",
            label: "bb0  entry",
            instructions: [
                .init(addr: "+0x0", mnemonic: "stp",  operands: "x29, x30, [sp, #-16]!"),
                .init(addr: "+0x4", mnemonic: "mov",  operands: "x29, sp"),
                .init(addr: "+0x8", mnemonic: "cbz",  operands: "x1, bb3"),
            ],
            rect: CGRect(x: 100, y: 20, width: 280, height: 70),
            hot: false
        ),
        CFGNode(
            id: "bb1",
            label: "bb1  loop_header",
            instructions: [
                .init(addr: "+0xc",  mnemonic: "mov",  operands: "x9, #0"),
                .init(addr: "+0x10", mnemonic: "cmp",  operands: "x9, x1"),
                .init(addr: "+0x14", mnemonic: "b.ge", operands: "bb3"),
            ],
            rect: CGRect(x: 100, y: 150, width: 280, height: 70),
            hot: true
        ),
        CFGNode(
            id: "bb2",
            label: "bb2  loop_body",
            instructions: [
                .init(addr: "+0x18", mnemonic: "ldr",  operands: "x10, [x0, x9, lsl #3]"),
                .init(addr: "+0x1c", mnemonic: "add",  operands: "x9, x9, #1"),
                .init(addr: "+0x20", mnemonic: "b",    operands: "bb1"),
            ],
            rect: CGRect(x: 100, y: 280, width: 280, height: 70),
            hot: true
        ),
        CFGNode(
            id: "bb3",
            label: "bb3  exit",
            instructions: [
                .init(addr: "+0x24", mnemonic: "ldp",  operands: "x29, x30, [sp], #16"),
                .init(addr: "+0x28", mnemonic: "ret",  operands: ""),
            ],
            rect: CGRect(x: 460, y: 150, width: 240, height: 56),
            hot: false
        ),
    ]

    private let edges: [CFGEdge] = [
        .init(from: "bb0", to: "bb1", kind: .fallThrough),
        .init(from: "bb0", to: "bb3", kind: .branch),
        .init(from: "bb1", to: "bb2", kind: .fallThrough),
        .init(from: "bb1", to: "bb3", kind: .branch),
        .init(from: "bb2", to: "bb1", kind: .backEdge),
    ]

    private var byId: [String: CGRect] {
        Dictionary(uniqueKeysWithValues: nodes.map { ($0.id, $0.rect) })
    }

    var body: some View {
        let canvasSize = CGSize(width: 740, height: 380)
        ScrollView([.horizontal, .vertical]) {
            ZStack(alignment: .topLeading) {
                Canvas { ctx, _ in
                    for edge in edges {
                        guard let f = byId[edge.from], let t = byId[edge.to] else { continue }
                        drawCFGEdge(from: f, to: t, kind: edge.kind, in: ctx)
                    }
                }
                .frame(width: canvasSize.width, height: canvasSize.height)

                ForEach(nodes) { node in
                    BasicBlockView(node: node)
                        .frame(width: node.rect.width, height: node.rect.height)
                        .offset(x: node.rect.minX, y: node.rect.minY)
                }
            }
            .frame(width: canvasSize.width, height: canvasSize.height)
            .padding(20)
        }
        .background(Color(nsColor: .textBackgroundColor).opacity(0.3))
    }
}

private struct BasicBlockView: View {
    let node: CFGNode

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text(node.label)
                .font(.mono(.caption))
                .foregroundStyle(.primary)
                .padding(.horizontal, 6)
                .padding(.vertical, 3)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(node.hot
                    ? Color.orange.opacity(0.25)
                    : Color.gray.opacity(0.22))

            VStack(alignment: .leading, spacing: 0) {
                ForEach(node.instructions, id: \.addr) { ins in
                    HStack(spacing: 6) {
                        Text(ins.addr)
                            .foregroundStyle(.tertiary)
                            .frame(width: 36, alignment: .trailing)
                        Text(ins.mnemonic)
                            .foregroundStyle(.primary)
                            .frame(width: 44, alignment: .leading)
                        Text(ins.operands)
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                    }
                    .font(.mono(.caption))
                    .frame(height: 14)
                }
            }
            .padding(.horizontal, 6)
            .padding(.vertical, 3)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .background(Color(nsColor: .textBackgroundColor))
        .clipShape(.rect(cornerRadius: 4))
        .overlay {
            RoundedRectangle(cornerRadius: 4)
                .stroke(Color.secondary.opacity(0.4), lineWidth: 0.5)
        }
    }
}

private func drawCFGEdge(from: CGRect, to: CGRect, kind: CFGEdgeKind, in ctx: GraphicsContext) {
    var path = Path()
    var endPoint: CGPoint = .zero
    var endTangent = CGVector(dx: 0, dy: 1)

    switch kind {
    case .fallThrough:
        let s = CGPoint(x: from.midX, y: from.maxY)
        let e = CGPoint(x: to.midX, y: to.minY)
        path.move(to: s)
        path.addLine(to: e)
        endPoint = e

    case .branch:
        // Detour right then down into the top of the destination.
        let s = CGPoint(x: from.maxX, y: from.midY)
        let e = CGPoint(x: to.midX, y: to.minY)
        let cp1 = CGPoint(x: s.x + 60, y: s.y)
        let cp2 = CGPoint(x: e.x, y: e.y - 50)
        path.move(to: s)
        path.addCurve(to: e, control1: cp1, control2: cp2)
        endPoint = e

    case .backEdge:
        // Detour left and up into the left edge of the destination.
        let s = CGPoint(x: from.minX, y: from.midY)
        let e = CGPoint(x: to.minX, y: to.midY)
        let cp1 = CGPoint(x: s.x - 70, y: s.y)
        let cp2 = CGPoint(x: e.x - 70, y: e.y)
        path.move(to: s)
        path.addCurve(to: e, control1: cp1, control2: cp2)
        endPoint = e
        endTangent = CGVector(dx: 1, dy: 0)
    }

    let color: Color = switch kind {
    case .fallThrough: .secondary
    case .branch:      .blue
    case .backEdge:    .orange
    }

    ctx.stroke(path, with: .color(color.opacity(0.85)), lineWidth: 1.2)
    drawArrowhead(at: endPoint, direction: endTangent, color: color, in: ctx)
}

private func drawArrowhead(at point: CGPoint, direction: CGVector, color: Color, in ctx: GraphicsContext) {
    let len = max(0.0001, (direction.dx * direction.dx + direction.dy * direction.dy).squareRoot())
    let ux = direction.dx / len
    let uy = direction.dy / len
    let head: CGFloat = 6
    var arrow = Path()
    arrow.move(to: point)
    arrow.addLine(to: CGPoint(
        x: point.x - ux * head + (-uy) * head / 2,
        y: point.y - uy * head + ( ux) * head / 2))
    arrow.addLine(to: CGPoint(
        x: point.x - ux * head - (-uy) * head / 2,
        y: point.y - uy * head - ( ux) * head / 2))
    arrow.closeSubpath()
    ctx.fill(arrow, with: .color(color.opacity(0.85)))
}
