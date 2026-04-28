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
        ZStack {
            Color(nsColor: .underPageBackgroundColor)
            Text("minimap")
                .font(.caption)
                .foregroundStyle(.tertiary)
        }
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
