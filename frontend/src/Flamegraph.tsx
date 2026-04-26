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
  onSelectAddress,
}: {
  client: ProfilerClient;
  onSelectAddress: (a: bigint) => void;
}) {
  const [update, setUpdate] = useState<FlamegraphUpdate | null>(null);
  const [hover, setHover] = useState<Box | null>(null);
  const containerRef = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    let cancelled = false;
    setUpdate(null);
    const [tx, rx] = channel<FlamegraphUpdate>();
    client.subscribeFlamegraph(tx).catch(() => {});
    (async () => {
      for await (const next of rx) {
        if (cancelled) break;
        setUpdate(next);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [client]);

  if (!update) {
    return <div className="flame placeholder">building flamegraph…</div>;
  }

  const { boxes, depth } = layout(update.root);
  const height = (depth + 1) * ROW_H;
  const total = update.total_samples;

  return (
    <div className="flame-wrap">
      <div
        ref={containerRef}
        className="flame"
        style={{ height: `${height}px` }}
        onMouseLeave={() => setHover(null)}
      >
        {boxes.map((b) => {
          const c = colorFor(b.node);
          const widthPct = (b.x1 - b.x0) * 100;
          return (
            <div
              key={b.key}
              className="flame-box"
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
              title={`${labelFor(b.node)} · ${b.node.count.toString()}/${total.toString()}`}
            >
              {widthPct > 2 ? labelFor(b.node) : ""}
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
              {hover.node.binary ? ` · ${hover.node.binary}` : ""}
            </span>
          </>
        ) : (
          <span className="flame-status-meta">
            {total.toString()} samples · click a box to open its disassembly
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
