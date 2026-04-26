import { useEffect, useRef, useState } from "react";
import { channel } from "@bearcove/vox-core";
import type {
  FlameNode,
  FlamegraphUpdate,
  LiveFilter,
  ProfilerClient,
} from "./generated/profiler.generated.ts";
import {
  objKindOf,
  viewParams,
  type ContextMenuTarget,
  type ObjKind,
} from "./App.tsx";

const ROW_H = 18;

type Box = {
  key: string;
  x0: number;
  x1: number;
  depth: number;
  node: FlameNode;
};

/// Class name for a flame box, picked from the node's kind. The
/// matching `.flame-box.kind-*` rules in CSS hold the actual colors,
/// so the boxes follow the active theme.
function kindClassFor(node: FlameNode): string {
  return `kind-${objKindOf(node)}`;
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
  filter,
  matchText,
  hiddenKinds,
  focusKey,
  onFocusKeyChange,
  onSelectAddress,
  onFrozenChange,
  onContextMenu,
  onDropSymbol,
}: {
  client: ProfilerClient;
  tid: number | null;
  filter: LiveFilter;
  matchText: ((t: string) => boolean) | null;
  hiddenKinds: Set<ObjKind>;
  focusKey: string | null;
  onFocusKeyChange: (k: string | null) => void;
  onSelectAddress: (a: bigint) => void;
  onFrozenChange?: (frozen: boolean) => void;
  onContextMenu: (t: ContextMenuTarget) => void;
  onDropSymbol: (s: { function_name: string | null; binary: string | null }) => void;
}) {
  const [update, setUpdate] = useState<FlamegraphUpdate | null>(null);
  const [hover, setHover] = useState<Box | null>(null);
  const [frozen, setFrozen] = useState(false);
  const containerRef = useRef<HTMLDivElement | null>(null);
  const frozenRef = useRef(false);
  const latestRef = useRef<FlamegraphUpdate | null>(null);
  const hoverRef = useRef<Box | null>(null);

  useEffect(() => {
    let cancelled = false;
    setUpdate(null);
    latestRef.current = null;
    onFocusKeyChange(null);
    const [tx, rx] = channel<FlamegraphUpdate>();
    client.subscribeFlamegraph(viewParams(tid, filter), tx).catch(() => {});
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
  }, [client, tid, filter]);

  useEffect(() => {
    frozenRef.current = frozen;
    if (!frozen && latestRef.current) {
      setUpdate(latestRef.current);
    }
    onFrozenChange?.(frozen);
  }, [frozen, onFrozenChange]);

  // F: focus the hovered subtree. D: drop the hovered symbol from
  // future aggregations. Only active while the cursor is inside the
  // flamegraph (frozen=true also means hovering).
  useEffect(() => {
    hoverRef.current = hover;
  }, [hover]);
  useEffect(() => {
    if (!frozen) return;
    const onKey = (e: KeyboardEvent) => {
      const h = hoverRef.current;
      if (!h || h.node.address === 0n) return;
      // Don't fire when the user is typing into a search/regex input.
      const target = e.target as HTMLElement | null;
      if (
        target &&
        (target.tagName === "INPUT" ||
          target.tagName === "TEXTAREA" ||
          target.isContentEditable)
      ) {
        return;
      }
      if (e.key === "f" || e.key === "F") {
        e.preventDefault();
        onFocusKeyChange(h.key);
      } else if (e.key === "d" || e.key === "D") {
        e.preventDefault();
        onDropSymbol({
          function_name: h.node.function_name,
          binary: h.node.binary,
        });
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [frozen, onFocusKeyChange, onDropSymbol]);

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
          const widthPct = (b.x1 - b.x0) * 100;
          const isMatch = nodeMatches(b.node, matchText);
          return (
            <div
              key={b.key}
              className={`flame-box ${kindClassFor(b.node)}${isMatch ? " match" : ""}`}
              style={{
                left: `${b.x0 * 100}%`,
                width: `${widthPct}%`,
                top: b.depth * ROW_H,
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
