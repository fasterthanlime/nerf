import { useEffect, useRef, useState } from "react";
import { channel } from "@bearcove/vox-core";
import type {
  FlameNode,
  NeighborsUpdate,
  ProfilerClient,
} from "./generated/profiler.generated.ts";
import {
  objKindOf,
  viewParams,
  type ContextMenuTarget,
  type ObjKind,
} from "./App.tsx";

const ROW_H = 16;

type Box = {
  key: string;
  x0: number;
  x1: number;
  depth: number;
  node: FlameNode;
};

function colorFor(node: FlameNode): { bg: string; fg: string } {
  if (node.is_main) return { bg: "#7fa86a", fg: "#0e1a07" };
  const b = node.binary ?? "";
  if (!b) return { bg: "#5a6066", fg: "#0e0f12" };
  if (b.startsWith("libsystem_") || b.startsWith("libobjc") || b.startsWith("dyld")) {
    return { bg: "#5e7a93", fg: "#06121f" };
  }
  return { bg: "#9b8453", fg: "#1a1208" };
}

function nodeMatches(n: FlameNode, matchText: ((t: string) => boolean) | null): boolean {
  if (!matchText) return false;
  return (
    (n.function_name != null && matchText(n.function_name)) ||
    (n.binary != null && matchText(n.binary))
  );
}

function layout(
  root: FlameNode,
  hiddenKinds: Set<ObjKind>,
  // Skip rendering the root node itself: its children are what we
  // care about (the target's neighbors), and the root just repeats
  // the function name we already show in the header.
  skipRoot: boolean,
): { boxes: Box[]; depth: number } {
  const boxes: Box[] = [];
  let maxDepth = 0;
  const startDepth = skipRoot ? -1 : 0;

  const walk = (
    node: FlameNode,
    depth: number,
    x0: number,
    x1: number,
    keyPrefix: string,
  ) => {
    if (x1 - x0 <= 0) return;
    if (depth >= 0) {
      boxes.push({ key: keyPrefix, x0, x1, depth, node });
      if (depth > maxDepth) maxDepth = depth;
    }
    const span = x1 - x0;
    const denom = node.count > 0n ? Number(node.count) : 1;
    let cursor = x0;
    node.children.forEach((c, i) => {
      const cw = (Number(c.count) / denom) * span;
      if (c.address !== 0n && hiddenKinds.has(objKindOf(c))) {
        cursor += cw;
        return;
      }
      walk(c, depth + 1, cursor, cursor + cw, `${keyPrefix}/${i}`);
      cursor += cw;
    });
  };
  walk(root, startDepth, 0, 1, "r");
  return { boxes, depth: Math.max(0, maxDepth) };
}

function labelFor(node: FlameNode): string {
  if (node.function_name) return node.function_name;
  if (node.address === 0n) return "(all)";
  return `0x${node.address.toString(16)}`;
}

function pct(count: bigint, total: bigint): string {
  if (total === 0n) return "0%";
  const r = Number((count * 10000n) / total) / 100;
  return `${r.toFixed(1)}%`;
}

/// One direction of the family tree (callers OR callees), rendered
/// as a small flamegraph. `flip` reverses the row order vertically so
/// callers stack above the target (kcachegrind style) while callees
/// stack below.
function FamilyChart({
  root,
  flip,
  matchText,
  hiddenKinds,
  onSelectAddress,
  onContextMenu,
  empty,
}: {
  root: FlameNode;
  flip: boolean;
  matchText: ((t: string) => boolean) | null;
  hiddenKinds: Set<ObjKind>;
  onSelectAddress: (a: bigint) => void;
  onContextMenu: (t: ContextMenuTarget) => void;
  empty: string;
}) {
  const { boxes, depth } = layout(root, hiddenKinds, true);
  const rows = depth + 1;
  const height = rows * ROW_H;
  const total = root.count;

  if (boxes.length === 0) {
    return <div className="family-empty">{empty}</div>;
  }
  return (
    <div className="family-chart" style={{ height: `${height}px` }}>
      {boxes.map((b) => {
        const c = colorFor(b.node);
        const widthPct = (b.x1 - b.x0) * 100;
        const isMatch = nodeMatches(b.node, matchText);
        const top = flip ? (depth - b.depth) * ROW_H : b.depth * ROW_H;
        return (
          <div
            key={b.key}
            className={`flame-box${isMatch ? " match" : ""}`}
            style={{
              left: `${b.x0 * 100}%`,
              width: `${widthPct}%`,
              top,
              background: c.bg,
              color: c.fg,
            }}
            onClick={() => {
              if (b.node.address !== 0n) onSelectAddress(b.node.address);
            }}
            onContextMenu={(e) => {
              e.preventDefault();
              if (b.node.address === 0n) return;
              onContextMenu({
                x: e.clientX,
                y: e.clientY,
                address: b.node.address,
                functionName: b.node.function_name,
                binary: b.node.binary,
                kind: objKindOf(b.node),
              });
            }}
            title={`${labelFor(b.node)} · ${b.node.count.toString()}/${total.toString()} · ${pct(b.node.count, total)}`}
          >
            {widthPct > 2 ? labelFor(b.node) : ""}
          </div>
        );
      })}
    </div>
  );
}

export function Neighbors({
  client,
  address,
  tid,
  matchText,
  hiddenKinds,
  onSelectAddress,
  onContextMenu,
}: {
  client: ProfilerClient;
  address: bigint;
  tid: number | null;
  matchText: ((t: string) => boolean) | null;
  hiddenKinds: Set<ObjKind>;
  onSelectAddress: (a: bigint) => void;
  onContextMenu: (t: ContextMenuTarget) => void;
}) {
  const [update, setUpdate] = useState<NeighborsUpdate | null>(null);
  const [frozen, setFrozen] = useState(false);
  const frozenRef = useRef(false);
  const latestRef = useRef<NeighborsUpdate | null>(null);

  useEffect(() => {
    let cancelled = false;
    setUpdate(null);
    latestRef.current = null;
    const [tx, rx] = channel<NeighborsUpdate>();
    client.subscribeNeighbors(address, viewParams(tid), tx).catch(() => {});
    (async () => {
      for await (const next of rx) {
        if (cancelled) break;
        latestRef.current = next;
        if (!frozenRef.current) setUpdate(next);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [client, address, tid]);

  useEffect(() => {
    frozenRef.current = frozen;
    if (!frozen && latestRef.current) {
      setUpdate(latestRef.current);
    }
  }, [frozen]);

  if (!update) {
    return <div className="neighbors placeholder">building family tree…</div>;
  }

  return (
    <div
      className="neighbors"
      onMouseEnter={() => setFrozen(true)}
      onMouseLeave={() => setFrozen(false)}
    >
      <div className="family-section">
        <div className="family-label">callers</div>
        <FamilyChart
          root={update.callers_tree}
          flip
          matchText={matchText}
          hiddenKinds={hiddenKinds}
          onSelectAddress={onSelectAddress}
          onContextMenu={onContextMenu}
          empty="(no caller info — sampled at top of stack)"
        />
      </div>
      <div className="family-target" title={update.binary ?? undefined}>
        <span className="family-target-label">
          {update.function_name ?? `0x${address.toString(16)}`}
        </span>
        <span className="family-target-meta">
          {update.own_count.toString()} samples
          {update.binary ? ` · ${update.binary}` : ""}
        </span>
      </div>
      <div className="family-section">
        <div className="family-label">callees</div>
        <FamilyChart
          root={update.callees_tree}
          flip={false}
          matchText={matchText}
          hiddenKinds={hiddenKinds}
          onSelectAddress={onSelectAddress}
          onContextMenu={onContextMenu}
          empty="(no callees — leaf function or kernel call)"
        />
      </div>
    </div>
  );
}
