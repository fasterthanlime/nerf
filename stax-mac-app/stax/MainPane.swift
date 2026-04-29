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
        if let fn = focusedFunction {
            VStack(spacing: 0) {
                NavHeader(model: model, focused: fn)
                Divider()
                CallGraphView(model: model, focused: fn)
            }
        } else {
            VStack(spacing: 0) {
                minimap
                Divider()
                flame
            }
        }
    }

    private var focusedFunction: AppModel.FunctionEntry? {
        guard let id = model.focusedFunctionId else { return nil }
        return model.functions.first { $0.id == id }
    }

    private var minimap: some View {
        Minimap(timeline: model.timeline)
            .frame(height: 56)
    }

    private var flame: some View {
        ZStack {
            Color(nsColor: .textBackgroundColor).opacity(0.3)
            Text("flame goes here")
                .font(.mono(.callout))
                .foregroundStyle(.tertiary)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
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
    let focused: AppModel.FunctionEntry

    var body: some View {
        HStack(spacing: 8) {
            Button {
                model.focusedFunctionId = nil
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
