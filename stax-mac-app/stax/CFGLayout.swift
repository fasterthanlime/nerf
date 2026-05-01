import Foundation

/// A layered (Sugiyama-style) layout of a CFG. The graph is small
/// (typically ≤ 100 blocks per function) so we keep all four phases
/// — back-edge classification, layer assignment, in-layer ordering,
/// coordinate assignment — in this one file.
///
/// Layout returns positions in *abstract* coordinates: column ∈ ℝ
/// inside a layer, row = layer index. The view layer multiplies by
/// fixed widths/heights to get pixel positions.
struct LaidOutCFG {
    let blocks: [LaidOutBlock]
    let edges: [LaidOutEdge]
    /// Width of the widest layer, in column units (each block is
    /// considered 1 column wide). The view multiplies this by the
    /// chosen block width to size the canvas.
    let columns: Double
    /// Number of layers, equal to `max(layer) + 1`.
    let layers: Int
}

struct LaidOutBlock: Identifiable {
    let id: UInt32
    let layer: Int
    /// Continuous column index — fractional values arise from the
    /// median heuristic. The renderer normalises with `columns`.
    let column: Double
    let block: BasicBlock
}

struct LaidOutEdge: Identifiable {
    let id: String
    let edge: CfgEdge
    /// `true` for edges that close a cycle (loop back-edges).
    /// Drawn with a distinct style.
    let isBackEdge: Bool
}

enum CFGLayout {
    static func layout(_ cfg: CfgUpdate) -> LaidOutCFG {
        let n = cfg.blocks.count
        guard n > 0 else {
            return LaidOutCFG(blocks: [], edges: [], columns: 0, layers: 0)
        }

        // Adjacency list keyed by block id. CFGs from server use dense
        // ids 0..n; we stay tolerant of sparse inputs by mapping ids
        // to indices.
        let idToIndex: [UInt32: Int] = Dictionary(
            uniqueKeysWithValues: cfg.blocks.enumerated().map { ($1.id, $0) }
        )
        var successors: [[Int]] = Array(repeating: [], count: n)
        var predecessors: [[Int]] = Array(repeating: [], count: n)
        var rawEdges: [(from: Int, to: Int, edge: CfgEdge)] = []
        for edge in cfg.edges {
            guard let f = idToIndex[edge.fromId], let t = idToIndex[edge.toId] else { continue }
            successors[f].append(t)
            predecessors[t].append(f)
            rawEdges.append((f, t, edge))
        }

        // Phase 1 — back-edge classification via DFS-grey detection.
        // Edges to a node currently on the DFS stack close a cycle
        // and are labelled back-edges. They participate in the final
        // graph but are excluded from layer assignment so the layered
        // layout sees a DAG.
        let backEdges = classifyBackEdges(n: n, successors: successors)

        // Phase 2 — longest-path layer assignment over the DAG (the
        // graph minus back-edges).
        let layers = assignLayers(
            n: n,
            successors: successors,
            backEdges: backEdges
        )
        let layerCount = (layers.max() ?? 0) + 1

        // Phase 3 — in-layer ordering via two passes of the median
        // heuristic. Initialise each layer with whatever order the
        // BFS happened to produce, then refine.
        var layerNodes: [[Int]] = Array(repeating: [], count: layerCount)
        for v in 0..<n {
            layerNodes[layers[v]].append(v)
        }
        for _ in 0..<2 {
            // Down pass: order each layer by median of predecessors.
            for l in 1..<layerCount {
                let above = layerNodes[l - 1]
                layerNodes[l].sort { a, b in
                    medianRank(of: a, neighbors: predecessors[a], inLayer: above)
                    < medianRank(of: b, neighbors: predecessors[b], inLayer: above)
                }
            }
            // Up pass: order each layer by median of successors.
            for l in stride(from: layerCount - 2, through: 0, by: -1) {
                let below = layerNodes[l + 1]
                layerNodes[l].sort { a, b in
                    medianRank(of: a, neighbors: successors[a], inLayer: below)
                    < medianRank(of: b, neighbors: successors[b], inLayer: below)
                }
            }
        }

        // Phase 4 — coordinate assignment. Each layer is centered
        // horizontally, blocks evenly spaced one column apart.
        let widest = layerNodes.map(\.count).max() ?? 0
        var columns: [Double] = Array(repeating: 0, count: n)
        for layer in layerNodes {
            let pad = Double(widest - layer.count) / 2.0
            for (i, v) in layer.enumerated() {
                columns[v] = pad + Double(i)
            }
        }

        let blocks: [LaidOutBlock] = (0..<n).map { idx in
            LaidOutBlock(
                id: cfg.blocks[idx].id,
                layer: layers[idx],
                column: columns[idx],
                block: cfg.blocks[idx]
            )
        }
        let edges: [LaidOutEdge] = rawEdges.enumerated().map { (i, e) in
            LaidOutEdge(
                id: "\(e.from)-\(e.to)-\(i)",
                edge: e.edge,
                isBackEdge: backEdges.contains(BackEdge(from: e.from, to: e.to))
            )
        }
        return LaidOutCFG(
            blocks: blocks,
            edges: edges,
            columns: Double(widest),
            layers: layerCount
        )
    }

    // MARK: - Back-edge classification

    private struct BackEdge: Hashable {
        let from: Int
        let to: Int
    }

    /// DFS from node 0 (the entry block), labelling any edge to a
    /// grey (currently-on-stack) node as a back-edge.
    private static func classifyBackEdges(n: Int, successors: [[Int]]) -> Set<BackEdge> {
        var color: [Int] = Array(repeating: 0, count: n)  // 0 white, 1 grey, 2 black
        var back: Set<BackEdge> = []
        var stack: [(node: Int, iter: Int)] = []
        stack.append((0, 0))
        color[0] = 1
        while let top = stack.last {
            let succs = successors[top.node]
            if top.iter < succs.count {
                stack[stack.count - 1].iter += 1
                let next = succs[top.iter]
                switch color[next] {
                case 0:
                    color[next] = 1
                    stack.append((next, 0))
                case 1:
                    back.insert(BackEdge(from: top.node, to: next))
                default:
                    break
                }
            } else {
                color[top.node] = 2
                stack.removeLast()
            }
        }
        return back
    }

    // MARK: - Layer assignment

    /// Longest-path layer assignment. Each node's layer is the
    /// longest forward-edge path from any source (a node with no
    /// non-back-edge predecessor). Topological order is computed via
    /// Kahn's algorithm; unreachable nodes (rare — usually means
    /// dead code) are placed in layer 0.
    private static func assignLayers(
        n: Int,
        successors: [[Int]],
        backEdges: Set<BackEdge>
    ) -> [Int] {
        var indegree: [Int] = Array(repeating: 0, count: n)
        for u in 0..<n {
            for v in successors[u] where !backEdges.contains(BackEdge(from: u, to: v)) {
                indegree[v] += 1
            }
        }
        var layers: [Int] = Array(repeating: 0, count: n)
        var queue: [Int] = (0..<n).filter { indegree[$0] == 0 }
        while !queue.isEmpty {
            let u = queue.removeFirst()
            for v in successors[u] where !backEdges.contains(BackEdge(from: u, to: v)) {
                if layers[u] + 1 > layers[v] {
                    layers[v] = layers[u] + 1
                }
                indegree[v] -= 1
                if indegree[v] == 0 {
                    queue.append(v)
                }
            }
        }
        return layers
    }

    // MARK: - Median ordering

    /// Median of `node`'s neighbour positions in the adjacent layer.
    /// Nodes with no neighbours keep their existing position by
    /// returning a sentinel large value (so they sink to the right).
    private static func medianRank(of node: Int, neighbors: [Int], inLayer layer: [Int])
        -> Double
    {
        if neighbors.isEmpty { return Double(layer.count) }
        let positions: [Int] = neighbors.compactMap { layer.firstIndex(of: $0) }.sorted()
        if positions.isEmpty { return Double(layer.count) }
        let m = positions.count / 2
        if positions.count % 2 == 1 {
            return Double(positions[m])
        }
        return (Double(positions[m - 1]) + Double(positions[m])) / 2.0
    }
}
