import { useEffect, useRef, useState } from "react";
import { channel } from "@bearcove/vox-core";
import type {
  FlamegraphUpdate as WireFlamegraphUpdate,
  LiveFilter,
  ProfilerClient,
} from "./generated/profiler.generated.ts";
import {
  hydrateFlamegraph,
  type FlamegraphView,
  type FlameView,
} from "./wire.ts";
import {
  langIcon,
  langOf,
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
  node: FlameView;
};

/// Class name for a flame box, picked from the node's kind. The
/// matching `.flame-box.kind-*` rules in CSS hold the actual colors,
/// so the boxes follow the active theme.
function kindClassFor(node: FlameView): string {
  return `kind-${objKindOf(node)}`;
}

function nodeMatches(
  n: FlameView,
  matchText: ((t: string) => boolean) | null,
): boolean {
  if (!matchText) return false;
  return (
    (n.function_name != null && matchText(n.function_name)) ||
    (n.binary != null && matchText(n.binary))
  );
}

/// Splice a relative box key (always rooted at "r" inside the
/// rendered subtree) onto an absolute parent key (rooted at "r" in
/// `update.root`). Used when the user pushes a focus while already
/// focused: the box's "r/x/y" layout key needs to become
/// "r/...parent.../x/y" so findByKey can resolve it from the live
/// root on the next render.
function combineKey(parentAbs: string | null, childRel: string): string {
  if (!parentAbs) return childRel;
  if (childRel === "r") return parentAbs;
  return parentAbs + childRel.slice(1); // drop the leading "r"
}

/// Find a node by its layout key (e.g. "r/2/1/0"). Mirrors the
/// `${keyPrefix}/${i}` numbering in `layout`.
function findByKey(node: FlameView, target: string): FlameView | null {
  if (target === "r") return node;
  const parts = target.split("/").slice(1); // drop the leading "r"
  let cur: FlameView = node;
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
  root: FlameView,
  hiddenKinds: Set<ObjKind>,
): { boxes: Box[]; depth: number } {
  const boxes: Box[] = [];
  let maxDepth = 0;
  const walk = (
    node: FlameView,
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
  currentAbsKey,
  onPushFocus,
  onPopDrill,
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
  /// Absolute flame key (relative to `update.root`) of the currently
  /// focused subtree, or null when there's no focus.
  currentAbsKey: string | null;
  /// Push a focus step onto the drill stack. The Flamegraph
  /// composes the absolute key (so callers don't have to know about
  /// the "r/.../..." encoding) and supplies a label suitable for a
  /// breadcrumb chip.
  onPushFocus: (step: { absKey: string; label: string; binary: string | null }) => void;
  /// Pop the most recent drill step (focus or exclude). Bound to
  /// Esc while the cursor is over the flamegraph.
  onPopDrill: () => void;
  onSelectAddress: (a: bigint) => void;
  onFrozenChange?: (frozen: boolean) => void;
  onContextMenu: (t: ContextMenuTarget) => void;
  onDropSymbol: (s: { function_name: string | null; binary: string | null }) => void;
}) {
  const [update, setUpdate] = useState<FlamegraphView | null>(null);
  const [hover, setHover] = useState<Box | null>(null);
  const [frozen, setFrozen] = useState(false);
  const containerRef = useRef<HTMLDivElement | null>(null);
  const frozenRef = useRef(false);
  const latestRef = useRef<FlamegraphView | null>(null);
  const hoverRef = useRef<Box | null>(null);

  // Persist the resize-handle height across reloads. Apply on mount,
  // observe size changes (CSS `resize: vertical` doesn't fire any
  // event by itself, but ResizeObserver picks up the dimensions),
  // and write back to localStorage. Wrapped in try/catch because
  // some embedding contexts throw on storage access.
  const FLAME_H_KEY = "nperf-flame-height";
  useEffect(() => {
    const el = containerRef.current;
    if (!el) return;
    try {
      const stored = localStorage.getItem(FLAME_H_KEY);
      if (stored) {
        const px = parseInt(stored, 10);
        if (Number.isFinite(px) && px >= 80) {
          el.style.height = `${px}px`;
        }
      }
    } catch {}
    const ro = new ResizeObserver((entries) => {
      for (const e of entries) {
        const h = Math.round(e.contentRect.height);
        if (h >= 80) {
          try {
            localStorage.setItem(FLAME_H_KEY, String(h));
          } catch {}
        }
      }
    });
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  useEffect(() => {
    let cancelled = false;
    setUpdate(null);
    latestRef.current = null;
    const [tx, rx] = channel<WireFlamegraphUpdate>();
    client.subscribeFlamegraph(viewParams(tid, filter), tx).catch(() => {});
    (async () => {
      for await (const wire of rx) {
        if (cancelled) break;
        const next = hydrateFlamegraph(wire);
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

  // F: focus the hovered subtree (push). D: drop the hovered symbol
  // from future aggregations. Esc: pop one drill step. Only active
  // while the cursor is inside the flamegraph (frozen=true means
  // hovering). Box keys delivered by `layout` are relative to the
  // rendered subtree; we splice the relative tail onto currentAbsKey
  // so the pushed focus key is still resolvable from update.root via
  // findByKey.
  useEffect(() => {
    hoverRef.current = hover;
  }, [hover]);
  useEffect(() => {
    if (!frozen) return;
    const onKey = (e: KeyboardEvent) => {
      const h = hoverRef.current;
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
      if (e.key === "Escape") {
        e.preventDefault();
        onPopDrill();
        return;
      }
      if (!h || h.node.address === 0n) return;
      if (e.key === "f" || e.key === "F") {
        e.preventDefault();
        onPushFocus({
          absKey: combineKey(currentAbsKey, h.key),
          label: labelFor(h.node),
          binary: h.node.binary,
        });
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
  }, [frozen, currentAbsKey, onPushFocus, onPopDrill, onDropSymbol]);

  // Pick the rendering root: focused subtree if set and findable,
  // otherwise the live root. When `update` is null we render an empty
  // shell so the outer `.flame` keeps its user-resized height across
  // filter changes (the ref + ResizeObserver stay attached).
  const renderRoot = update
    ? currentAbsKey
      ? findByKey(update.root, currentAbsKey) ?? update.root
      : update.root
    : null;
  const { boxes, depth } = renderRoot
    ? layout(renderRoot, hiddenKinds)
    : { boxes: [], depth: 0 };
  const innerHeight = (depth + 1) * ROW_H;
  const total = update?.total_samples ?? 0n;

  return (
    <div
      className="flame-wrap"
      onMouseEnter={() => setFrozen(true)}
      onMouseLeave={() => {
        setFrozen(false);
        setHover(null);
      }}
    >
      <div ref={containerRef} className="flame">
        <div className="flame-inner" style={{ height: `${innerHeight}px` }} />
        {!update && (
          <div className="flame-placeholder">building flamegraph…</div>
        )}
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
                  flameKey: combineKey(currentAbsKey, b.key),
                });
              }}
              title={tooltipFor(b.node, total)}
            >
              {widthPct > 2 ? <FlameBoxLabel node={b.node} /> : null}
            </div>
          );
        })}
      </div>
      <div className="flame-status">
        {hover ? (
          <>
            <span className="flame-status-label">{labelFor(hover.node)}</span>
            <span className="flame-status-meta">
              {hover.node.count.toString()} / {total.toString()} ·{" "}
              {pct(hover.node.count, total)}
              {ipcFor(hover.node) ? ` · ${ipcFor(hover.node)} ipc` : ""}
              {hover.node.binary ? ` · ${hover.node.binary}` : ""}
            </span>
          </>
        ) : update ? (
          <span className="flame-status-meta">
            {total.toString()} samples · click to open · right-click to focus
          </span>
        ) : (
          <span className="flame-status-meta">building flamegraph…</span>
        )}
      </div>
    </div>
  );
}

function labelFor(node: FlameView): string {
  if (node.function_name) return node.function_name;
  if (node.address === 0n) return "(all)";
  return `0x${node.address.toString(16)}`;
}

/// Box content: language icon, symbol name, then a dimmed binary
/// basename. Same overall shape as the top-table row, just inline on
/// one line so it fits inside a 17px flame box. Sub-pixel boxes are
/// blanked out by the caller; this component handles everything from
/// "narrow but visible" up.
function FlameBoxLabel({ node }: { node: FlameView }) {
  const lang = langOf(node);
  return (
    <>
      <span className={`glyph lang-${lang}`}>{langIcon(lang)}</span>
      <span className="fn-name">{labelFor(node)}</span>
      {node.binary ? (
        <span className="bin-name">{node.binary}</span>
      ) : null}
    </>
  );
}

function pct(count: bigint, total: bigint): string {
  if (total === 0n) return "0%";
  const r = Number((count * 10000n) / total) / 100;
  return `${r.toFixed(1)}%`;
}

/// Inclusive IPC for a flame node, formatted to two decimals. `null`
/// when the kperf backend didn't report PMU values for this run.
function ipcFor(node: FlameView): string | null {
  if (node.cycles === 0n) return null;
  const ipc = Number(node.instructions) / Number(node.cycles);
  return ipc.toFixed(2);
}

function tooltipFor(node: FlameView, total: bigint): string {
  const base = `${labelFor(node)} · ${node.count.toString()}/${total.toString()}`;
  const ipc = ipcFor(node);
  return ipc
    ? `${base} · ${ipc} ipc (${node.instructions.toString()} insns / ${node.cycles.toString()} cycles)`
    : base;
}
