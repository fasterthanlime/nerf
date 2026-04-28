import SwiftUI

struct CallGraphView: View {
    @Bindable var model: AppModel
    let focused: AppModel.FunctionEntry

    private let nodeWidth: CGFloat = 360
    private let nodeHeight: CGFloat = 36
    private let vGap: CGFloat = 28

    var body: some View {
        let nodes = makeNodes()
        let canvasW: CGFloat = nodeWidth + 80
        let canvasH = CGFloat(nodes.count) * (nodeHeight + vGap) + 40
        let positions = layout(nodes: nodes, canvasW: canvasW)
        let maxTime = max(0.000_001, nodes.map(\.member.totalTime).max() ?? 0)

        ScrollView([.horizontal, .vertical]) {
            ZStack(alignment: .topLeading) {
                Canvas { ctx, _ in
                    for i in 0..<positions.count - 1 {
                        drawCallEdge(from: positions[i], to: positions[i + 1], in: ctx)
                    }
                }
                .frame(width: canvasW, height: canvasH)

                ForEach(0..<nodes.count, id: \.self) { i in
                    CallNodeView(
                        member: nodes[i].member,
                        role: nodes[i].role,
                        maxTime: maxTime
                    )
                    .frame(width: nodeWidth, height: nodeHeight)
                    .offset(x: positions[i].minX, y: positions[i].minY)
                }
            }
            .frame(width: canvasW, height: canvasH)
            .padding(20)
        }
        .background(Color(nsColor: .textBackgroundColor).opacity(0.3))
    }

    private func makeNodes() -> [CallNode] {
        var out: [CallNode] = []
        out += model.familyCallers.map { CallNode(member: $0, role: .caller) }
        out.append(CallNode(member: focusedMember, role: .focused))
        out += model.familyCallees.map { CallNode(member: $0, role: .callee) }
        return out
    }

    private var focusedMember: AppModel.FamilyMember {
        AppModel.FamilyMember(
            name: focused.name,
            binary: focused.binary,
            kind: focused.kind,
            totalTime: focused.totalTime > 0 ? focused.totalTime : model.familyFocused.totalTime,
            callCount: 1
        )
    }

    private func layout(nodes: [CallNode], canvasW: CGFloat) -> [CGRect] {
        let x = (canvasW - nodeWidth) / 2
        return nodes.enumerated().map { i, _ in
            CGRect(
                x: x,
                y: 20 + CGFloat(i) * (nodeHeight + vGap),
                width: nodeWidth,
                height: nodeHeight
            )
        }
    }
}

private struct CallNode {
    let member: AppModel.FamilyMember
    let role: CallRole
}

private enum CallRole { case caller, focused, callee }

private struct CallNodeView: View {
    let member: AppModel.FamilyMember
    let role: CallRole
    let maxTime: TimeInterval

    var body: some View {
        HStack(spacing: 6) {
            LanguageBadge(kind: member.kind, size: 12)
            Text(member.name)
                .font(.mono(.caption))
                .foregroundStyle(role == .focused ? .primary : .secondary)
                .lineLimit(1)
            Spacer(minLength: 6)
            BarTrack(ratio: member.totalTime / maxTime)
                .frame(width: 50, height: 4)
            Text(formatDuration(member.totalTime))
                .font(.mono(.caption))
                .foregroundStyle(.secondary)
                .frame(width: 56, alignment: .trailing)
            Text("\(member.callCount)\u{00D7}")
                .font(.mono(.caption2))
                .foregroundStyle(.tertiary)
                .frame(width: 24, alignment: .trailing)
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 4)
        .background(background)
        .overlay {
            RoundedRectangle(cornerRadius: 4)
                .stroke(borderColor, lineWidth: role == .focused ? 1 : 0.5)
        }
        .clipShape(.rect(cornerRadius: 4))
    }

    private var background: Color {
        switch role {
        case .focused: Color.accentColor.opacity(0.18)
        default:       Color(nsColor: .textBackgroundColor)
        }
    }

    private var borderColor: Color {
        switch role {
        case .focused: Color.accentColor.opacity(0.7)
        default:       Color.secondary.opacity(0.4)
        }
    }
}

private func drawCallEdge(from: CGRect, to: CGRect, in ctx: GraphicsContext) {
    let s = CGPoint(x: from.midX, y: from.maxY)
    let e = CGPoint(x: to.midX, y: to.minY)
    var path = Path()
    path.move(to: s)
    path.addLine(to: e)
    ctx.stroke(path, with: .color(.secondary.opacity(0.6)), lineWidth: 1)

    var arrow = Path()
    arrow.move(to: e)
    arrow.addLine(to: CGPoint(x: e.x - 4, y: e.y - 6))
    arrow.addLine(to: CGPoint(x: e.x + 4, y: e.y - 6))
    arrow.closeSubpath()
    ctx.fill(arrow, with: .color(.secondary.opacity(0.7)))
}
