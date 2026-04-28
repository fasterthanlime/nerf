import AppKit
import SwiftUI

struct ContentView: View {
    @State private var model = AppModel()
    @State private var columnVisibility: NavigationSplitViewVisibility = .detailOnly

    var body: some View {
        NavigationSplitView(columnVisibility: $columnVisibility) {
            FilterSidebar(model: model)
                .navigationSplitViewColumnWidth(min: 200, ideal: 240, max: 320)
        } detail: {
            VStack(spacing: 0) {
                MainPane(model: model)
                StatusBar(model: model)
            }
            .frame(minWidth: 600, minHeight: 400)
        }
        .toolbar {
            ToolbarItem(placement: .navigation) {
                Button {
                    model.paused.toggle()
                } label: {
                    Image(systemName: model.paused ? "play.fill" : "pause.fill")
                }
                .help(model.paused ? "Resume" : "Pause")
            }
            ToolbarItem(placement: .navigation) {
                ThreadPicker(model: model)
            }
            ToolbarItem(placement: .primaryAction) {
                NativeSearchField(text: $model.searchQuery, prompt: #"search symbols (try "exact" or /regex/)"#)
                    .frame(width: 280)
            }
        }
    }
}

struct NativeSearchField: NSViewRepresentable {
    @Binding var text: String
    var prompt: String

    func makeNSView(context: Context) -> NSSearchField {
        let field = NSSearchField()
        field.placeholderString = prompt
        field.delegate = context.coordinator
        field.controlSize = .small
        field.bezelStyle = .roundedBezel
        field.sendsSearchStringImmediately = true
        field.sendsWholeSearchString = false
        return field
    }

    func updateNSView(_ nsView: NSSearchField, context: Context) {
        if nsView.stringValue != text {
            nsView.stringValue = text
        }
    }

    func makeCoordinator() -> Coordinator { Coordinator(self) }

    final class Coordinator: NSObject, NSSearchFieldDelegate {
        var parent: NativeSearchField
        init(_ parent: NativeSearchField) { self.parent = parent }

        func controlTextDidChange(_ obj: Notification) {
            guard let field = obj.object as? NSSearchField else { return }
            parent.text = field.stringValue
        }
    }
}

#Preview {
    ContentView()
        .frame(width: 1200, height: 700)
}
