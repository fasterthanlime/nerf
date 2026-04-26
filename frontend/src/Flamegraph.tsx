import { useEffect, useRef, useState } from "react";
import { channel } from "@bearcove/vox-core";
import type {
  FlameNode,
  FlamegraphUpdate,
  ProfilerClient,
} from "./generated/profiler.generated.ts";
import { objKindOf, type ContextMenuTarget, type ObjKind } from "./App.tsx";

const ROW_H = 18;

type Box = {
  key: string;
  x0: number;
  x1: number;
  depth: number;
  node: FlameNode;
};

type Color = { bg: string; fg: string };

/// Pick a color for a box based on `is_main` + binary kind.
function colorFor(node: FlameNode): Color {
  if (node.is_main) return { bg: "#7fa86a", fg: "#0e1a07" };
  const b = node.binary ?? "";
  if (!b) return { bg: "#5a6066", fg: "#0e0f12" };
  if (
    b.startsWith("libsystem_") ||
    b.startsWith("libobjc") ||
    b.startsWith("dyld")
  ) {
    return { bg: "#5e7a93", fg: "#06121f" };
  }
  return { bg: "#9b8453", fg: "#1a1208" };
}

function nodeMatches(
  n: FlameNode,
  matchText: ((t: string) => boolean) | null,
): boolean {
  if (!matchText) return false;
  return (
    (n.function_name != null && matchText(n.function_name)) ||
    (n.binary != null && matchText(n.binary))
  );
}

/// Find a node by its layout key (e.g. "r/2/1/0"). Mirrors the
/// `${keyPrefix}/${i}` numbering in `layout`.
function findByKey(node: FlameNode, target: string): FlameNode | null {
  if (target === "r") return node;
  const parts = target.split("/").slice(1); // drop the leading "r"
  let cur: FlameNode = node;
  for (const p of parts) {
    const i = Number(p);
    if (!Number.isFinite(i) || i < 0 || i >= cur.children.length) return null;
    cur = cur.children[i];
  }
  return cur;
}

/// Layout the tree into [0,1] horizontal coordinate space. Subtrees
/// rooted at a node whose kind is in `hiddenKinds` are pruned (their
/// box is omitted *and* their descendants are skipped); the parent's
/// box keeps its width and just gets fewer / no children stacked on
/// top.
function layout(
  root: FlameNode,
  hiddenKinds: Set<ObjKind>,
): { boxes: Box[]; depth: number } {
  const boxes: Box[] = [];
  let maxDepth = 0;
  const walk = (
    node: FlameNode,
    depth: number,
    x0: number,
    x1: number,
    keyPrefix: string,
  ) => {
    if (x1 - x0 <= 0) return;
    boxes.push({
      key: keyPrefix,
      x0,
      x1,
      depth,
      node,
    });
    if (depth > maxDepth) maxDepth = depth;
    const span = x1 - x0;
    const denom = node.count > 0n ? Number(node.count) : 1;
    let cursor = x0;
    node.children.forEach((c, i) => {
      const cw = (Number(c.count) / denom) * span;
      // Address 0 = synthetic root marker; never filter that out.
      if (c.address !== 0n && hiddenKinds.has(objKindOf(c))) {
        cursor += cw;
        return;
      }
      walk(c, depth + 1, cursor, cursor + cw, `${keyPrefix}/${i}`);
      cursor += cw;
    });
  };
  walk(root, 0, 0, 1, "r");
  return { boxes, depth: maxDepth };
}

export function Flamegraph({
  client,
  tid,
  matchText,
  hiddenKinds,
  focusKey,
  onFocusKeyChange,
  onSelectAddress,
  onFrozenChange,
  onContextMenu,
}: {
  client: ProfilerClient;
  tid: number | null;
  matchText: ((t: string) => boolean) | null;
  hiddenKinds: Set<ObjKind>;
  focusKey: string | null;
  onFocusKeyChange: (k: string | null) => void;
  onSelectAddress: (a: bigint) => void;
  onFrozenChange?: (frozen: boolean) => void;
  onContextMenu: (t: ContextMenuTarget) => void;
}) {
  const [update, setUpdate] = useState<FlamegraphUpdate | null>(null);
  const [hover, setHover] = useState<Box | null>(null);
  const [frozen, setFrozen] = useState(false);
  const containerRef = useRef<HTMLDivElement | null>(null);
  const frozenRef = useRef(false);
  const latestRef = useRef<FlamegraphUpdate | null>(null);

  useEffect(() => {
    let cancelled = false;
    setUpdate(null);
    latestRef.current = null;
    onFocusKeyChange(null);
    const [tx, rx] = channel<FlamegraphUpdate>();
    client.subscribeFlamegraph(tid, tx).catch(() => {});
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
  }, [client, tid]);

  useEffect(() => {
    frozenRef.current = frozen;
    if (!frozen && latestRef.current) {
      setUpdate(latestRef.current);
    }
    onFrozenChange?.(frozen);
  }, [frozen, onFrozenChange]);

  if (!update) {
    return <div className="flame placeholder">building flamegraph…</div>;
  }

  // Pick the rendering root: focused subtree if set and findable,
  // otherwise the live root.
  const renderRoot = focusKey
    ? findByKey(update.root, focusKey) ?? update.root
    : update.root;
  const { boxes, depth } = layout(renderRoot, hiddenKinds);
  const height = (depth + 1) * ROW_H;
  const total = update.total_samples;

  return (
    <div
      className="flame-wrap"
      onMouseEnter={() => setFrozen(true)}
      onMouseLeave={() => {
        setFrozen(false);
        setHover(null);
      }}
    >
      <div
        ref={containerRef}
        className="flame"
        style={{ height: `${height}px` }}
      >
        {boxes.map((b) => {
          const c = colorFor(b.node);
          const widthPct = (b.x1 - b.x0) * 100;
          const isMatch = nodeMatches(b.node, matchText);
          return (
            <div
              key={b.key}
              className={`flame-box${isMatch ? " match" : ""}`}
              style={{
                left: `${b.x0 * 100}%`,
                width: `${widthPct}%`,
                top: b.depth * ROW_H,
                background: c.bg,
                color: c.fg,
              }}
              onMouseEnter={() => setHover(b)}
              onClick={() => {
                if (b.node.address !== 0n) onSelectAddress(b.node.address);
              }}
              onContextMenu={(e) => {
                e.preventDefault();
                onContextMenu({
                  x: e.clientX,
                  y: e.clientY,
                  address: b.node.address,
                  functionName: b.node.function_name,
                  binary: b.node.binary,
                  kind: b.node.address === 0n ? undefined : objKindOf(b.node),
                  flameKey: b.key,
                });
              }}
              title={`${labelFor(b.node)} · ${b.node.count.toString()}/${total.toString()}`}
            >
              {widthPct > 2 ? labelFor(b.node) : ""}
            </div>
          );
        })}
      </div>
      <div className="flame-status">
        {focusKey && (
          <button
            className="flame-reset"
            onClick={() => onFocusKeyChange(null)}
            title="clear focus and show the full tree"
          >
            ↩ reset focus
          </button>
        )}
        {hover ? (
          <>
            <span className="flame-status-label">{labelFor(hover.node)}</span>
            <span className="flame-status-meta">
              {hover.node.count.toString()} / {total.toString()} ·{" "}
              {pct(hover.node.count, total)}
              {hover.node.binary ? ` · ${hover.node.binary}` : ""}
            </span>
          </>
        ) : (
          <span className="flame-status-meta">
            {total.toString()} samples · click to open · right-click to focus
          </span>
        )}
      </div>
    </div>
  );
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
