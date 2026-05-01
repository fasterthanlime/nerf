import SwiftUI

struct FilterSidebar: View {
    @Bindable var model: AppModel

    var body: some View {
        List {
            Section {
                ForEach(AppModel.CPUMode.allCases) { mode in
                    ModeRow(
                        label: mode.rawValue,
                        isOn: model.cpuMode == mode
                    ) {
                        model.cpuMode = mode
                    }
                }
            } header: {
                SectionHeader("mode")
            }

            Section {
                ForEach(AppModel.EventMode.allCases) { mode in
                    EventRow(
                        label: mode.rawValue,
                        isOn: model.eventMode == mode
                    ) {
                        model.eventMode = (model.eventMode == mode) ? nil : mode
                    }
                }
            } header: {
                SectionHeader("events")
            }

            Section {
                ForEach(AppModel.Category.allCases) { cat in
                    CategoryRow(
                        category: cat,
                        isOn: model.categories.contains(cat)
                    ) {
                        if model.categories.contains(cat) {
                            model.categories.remove(cat)
                        } else {
                            model.categories.insert(cat)
                        }
                    }
                }
            } header: {
                SectionHeader("categories")
            }

            Section {
                ThreadRow(
                    name: "all threads",
                    detail: formatDuration(model.totalThreadOnCPU),
                    ratio: nil,
                    isSelected: model.threadFilter == nil
                ) {
                    model.threadFilter = nil
                }
                ForEach(model.threadsSorted) { thread in
                    ThreadRow(
                        name: thread.displayName,
                        detail: formatDuration(thread.onCPU),
                        ratio: thread.onCPU / model.maxThreadOnCPU,
                        isSelected: model.threadFilter == thread.tid
                    ) {
                        model.threadFilter = thread.tid
                    }
                }
            } header: {
                SectionHeader("threads")
            }
        }
        .listStyle(.sidebar)
    }
}

private struct SectionHeader: View {
    let label: String
    init(_ label: String) { self.label = label }

    var body: some View {
        Text(label)
            .font(.mono(.caption))
            .foregroundStyle(.tertiary)
            .textCase(nil)
    }
}

private struct ModeRow: View {
    let label: String
    let isOn: Bool
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack(spacing: 8) {
                Image(systemName: isOn ? "largecircle.fill.circle" : "circle")
                    .foregroundStyle(isOn ? Color.accentColor : .secondary)
                Text(label)
                    .foregroundStyle(isOn ? .primary : .secondary)
                Spacer()
            }
            .contentShape(.rect)
        }
        .buttonStyle(.plain)
    }
}

private struct EventRow: View {
    let label: String
    let isOn: Bool
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack(spacing: 8) {
                Image(systemName: isOn ? "checkmark.square.fill" : "square")
                    .foregroundStyle(isOn ? Color.accentColor : .secondary)
                Text(label)
                    .foregroundStyle(isOn ? .primary : .secondary)
                Spacer()
            }
            .contentShape(.rect)
        }
        .buttonStyle(.plain)
    }
}

private struct CategoryRow: View {
    let category: AppModel.Category
    let isOn: Bool
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack(spacing: 8) {
                RoundedRectangle(cornerRadius: 2)
                    .fill(category.color)
                    .opacity(isOn ? 1 : 0.2)
                    .frame(width: 12, height: 12)
                    .overlay {
                        RoundedRectangle(cornerRadius: 2)
                            .stroke(category.color.opacity(isOn ? 0 : 0.5), lineWidth: 0.5)
                    }
                Text(category.rawValue)
                    .foregroundStyle(isOn ? .primary : .secondary)
                Spacer()
            }
            .contentShape(.rect)
        }
        .buttonStyle(.plain)
    }
}

private struct ThreadRow: View {
    let name: String
    let detail: String
    let ratio: Double?
    let isSelected: Bool
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack(spacing: 6) {
                Image(systemName: "cpu")
                    .foregroundStyle(isSelected ? Color.accentColor : .secondary)
                    .font(.caption)
                    .frame(width: 12)
                Text(name)
                    .foregroundStyle(isSelected ? .primary : .secondary)
                    .lineLimit(1)
                Spacer(minLength: 4)
                Group {
                    if let r = ratio {
                        BarTrack(ratio: r)
                    } else {
                        Color.clear
                    }
                }
                .frame(width: 32, height: 3)
                Text(detail)
                    .font(.mono(.caption))
                    .foregroundStyle(.tertiary)
                    .frame(width: 56, alignment: .trailing)
            }
            .contentShape(.rect)
        }
        .buttonStyle(.plain)
    }
}
