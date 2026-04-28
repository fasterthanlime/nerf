import SwiftUI

enum SymbolKind: String, Hashable {
    case rust, c, cpp, swift, objc, unknown

    var assetName: String? {
        switch self {
        case .rust:    "Languages/rust"
        case .c:       "Languages/c"
        case .cpp:     "Languages/cplusplus"
        case .swift:   "Languages/swift"
        case .objc:    "Languages/objectivec"
        case .unknown: nil
        }
    }
}

struct LanguageBadge: View {
    let kind: SymbolKind
    var size: CGFloat = 14

    var body: some View {
        Group {
            if let name = kind.assetName {
                Image(name)
                    .renderingMode(.template)
                    .resizable()
                    .interpolation(.high)
                    .scaledToFit()
            } else {
                Image(systemName: "questionmark.circle.fill")
                    .resizable()
                    .scaledToFit()
            }
        }
        .foregroundStyle(.secondary)
        .frame(width: size, height: size)
    }
}
