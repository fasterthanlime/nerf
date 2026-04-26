import { useEffect, useRef, useState } from "react";
import { channel } from "@bearcove/vox-core";
import type {
  FlameNode,
  FlamegraphUpdate,
  ProfilerClient,
} from "./generated/profiler.generated.ts";

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

function nodeMatches(n: FlameNode, search: string): boolean {
  if (!search) return false;
  const term = search.toLowerCase();
  return (
    (n.function_name?.toLowerCase().includes(term) ?? false) ||
    (n.binary?.toLowerCase().includes(term) ?? false)
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

/// Layout the tree into [0,1] horizontal coordinate space.
function layout(root: FlameNode): { boxes: Box[]; depth: number } {
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
  search,
  onSelectAddress,
  onFrozenChange,
}: {
  client: ProfilerClient;
  tid: number | null;
  search: string;
  onSelectAddress: (a: bigint) => void;
  onFrozenChange?: (frozen: boolean) => void;
}) {
  const [update, setUpdate] = useState<FlamegraphUpdate | null>(null);
  const [hover, setHover] = useState<Box | null>(null);
  const [frozen, setFrozen] = useState(false);
  const [focusKey, setFocusKey] = useState<string | null>(null);
  const [menu, setMenu] = useState<{
    x: number;
    y: number;
    box: Box;
  } | null>(null);
  const containerRef = useRef<HTMLDivElement | null>(null);
  const frozenRef = useRef(false);
  const latestRef = useRef<FlamegraphUpdate | null>(null);

  useEffect(() => {
    let cancelled = false;
    setUpdate(null);
    latestRef.current = null;
    setFocusKey(null);
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

  // Close the context menu on any outside click / scroll / key.
  useEffect(() => {
    if (!menu) return;
    const close = () => setMenu(null);
    window.addEventListener("click", close);
    window.addEventListener("scroll", close, true);
    window.addEventListener("keydown", close);
    return () => {
      window.removeEventListener("click", close);
      window.removeEventListener("scroll", close, true);
      window.removeEventListener("keydown", close);
    };
  }, [menu]);

  if (!update) {
    return <div className="flame placeholder">building flamegraph…</div>;
  }

  // Pick the rendering root: focused subtree if set and findable,
  // otherwise the live root.
  const renderRoot = focusKey
    ? findByKey(update.root, focusKey) ?? update.root
    : update.root;
  const { boxes, depth } = layout(renderRoot);
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
          const isMatch = nodeMatches(b.node, search);
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
                setMenu({ x: e.clientX, y: e.clientY, box: b });
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
            onClick={() => setFocusKey(null)}
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
      {menu && (
        <div
          className="context-menu"
          style={{ top: menu.y, left: menu.x }}
          // stop the outer window click handler from immediately closing us
          onMouseDown={(e) => e.stopPropagation()}
          onClick={(e) => e.stopPropagation()}
        >
          <button
            onClick={() => {
              setFocusKey(menu.box.key);
              setMenu(null);
            }}
          >
            Focus this subtree
          </button>
          <button
            onClick={() => {
              if (menu.box.node.address !== 0n) {
                onSelectAddress(menu.box.node.address);
              }
              setMenu(null);
            }}
          >
            Open disassembly
          </button>
        </div>
      )}
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
