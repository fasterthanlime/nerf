import { useEffect, useRef, useState } from "react";
import { channel } from "@bearcove/vox-core";
import type {
  ProfilerClient,
  TimeRange,
  TimelineUpdate,
} from "./generated/profiler.generated.ts";

/// Compact timeline strip across the top of the page. Each bucket is
/// drawn as a vertical bar; bar height is proportional to the bucket's
/// sample count relative to the busiest bucket in view. Drag across
/// the bars to brush-select a time range; click to clear.
export function Timeline({
  client,
  tid,
  range,
  onRangeChange,
}: {
  client: ProfilerClient;
  tid: number | null;
  range: TimeRange | null;
  onRangeChange: (r: TimeRange | null) => void;
}) {
  const [update, setUpdate] = useState<TimelineUpdate | null>(null);
  const barsRef = useRef<SVGSVGElement | null>(null);
  /// Live drag state: start/current x as fractions of the bars width.
  /// `null` when the user isn't currently dragging.
  const [drag, setDrag] = useState<{ x0: number; x1: number } | null>(null);

  useEffect(() => {
    let cancelled = false;
    setUpdate(null);
    const [tx, rx] = channel<TimelineUpdate>();
    client.subscribeTimeline(tid, tx).catch(() => {});
    (async () => {
      for await (const next of rx) {
        if (cancelled) break;
        setUpdate(next);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [client, tid]);

  if (!update || update.buckets.length === 0) {
    return <div className="timeline placeholder">timeline (waiting for samples…)</div>;
  }

  const max = update.buckets.reduce(
    (m, b) => (b.count > m ? b.count : m),
    0n,
  );
  const maxF = max === 0n ? 1 : Number(max);
  const durSec = Number(update.duration_ns) / 1e9;
  const durNs = update.duration_ns;

  /// Map a clientX coordinate inside `barsRef` to a [0,1] fraction
  /// along the bars row, clamped to the visible area.
  const fracOf = (clientX: number): number => {
    const el = barsRef.current;
    if (!el) return 0;
    const rect = el.getBoundingClientRect();
    if (rect.width <= 0) return 0;
    return Math.max(0, Math.min(1, (clientX - rect.left) / rect.width));
  };

  const fracToNs = (f: number): bigint => {
    if (durNs === 0n) return 0n;
    return BigInt(Math.round(f * Number(durNs)));
  };

  const onMouseDown = (e: React.MouseEvent) => {
    if (e.button !== 0) return;
    e.preventDefault();
    const f = fracOf(e.clientX);
    setDrag({ x0: f, x1: f });
  };
  const onMouseMove = (e: React.MouseEvent) => {
    if (!drag) return;
    setDrag({ x0: drag.x0, x1: fracOf(e.clientX) });
  };
  const finishDrag = (e: React.MouseEvent) => {
    if (!drag) return;
    const x1 = fracOf(e.clientX);
    setDrag(null);
    const lo = Math.min(drag.x0, x1);
    const hi = Math.max(drag.x0, x1);
    // Treat tiny drags (< ~1% of width) as clicks → clear the range.
    if (hi - lo < 0.005) {
      if (range) onRangeChange(null);
      return;
    }
    onRangeChange({ start_ns: fracToNs(lo), end_ns: fracToNs(hi) });
  };

  // Selection overlay — prefer the live drag state; fall back to the
  // committed range converted to fractions.
  const overlay = drag
    ? { lo: Math.min(drag.x0, drag.x1), hi: Math.max(drag.x0, drag.x1) }
    : range && durNs > 0n
      ? {
          lo: Number(range.start_ns) / Number(durNs),
          hi: Number(range.end_ns) / Number(durNs),
        }
      : null;

  // Build the area-chart path. Each bucket center sits at
  // (i + 0.5) / N along x; y is inverted (0 at top, 100 at bottom).
  // We start at the bottom-left, climb to each bucket's height,
  // then close back at the bottom-right -- producing a single
  // filled area instead of the discrete bars we used to draw.
  const n = update.buckets.length;
  const points: string[] = [];
  for (let i = 0; i < n; i++) {
    const x = ((i + 0.5) / n) * 100;
    const y = max === 0n ? 100 : 100 - (Number(update.buckets[i].count) / maxF) * 100;
    points.push(`${x.toFixed(3)},${y.toFixed(3)}`);
  }
  const areaD =
    n === 0
      ? ""
      : `M 0,100 L ${points.join(" L ")} L 100,100 Z`;

  return (
    <div className="timeline">
      <svg
        ref={barsRef}
        className="timeline-graph"
        viewBox="0 0 100 100"
        preserveAspectRatio="none"
        onMouseDown={onMouseDown}
        onMouseMove={onMouseMove}
        onMouseUp={finishDrag}
        onMouseLeave={(e) => {
          if (drag) finishDrag(e);
        }}
      >
        {areaD && <path className="timeline-area" d={areaD} />}
        {overlay && (
          <rect
            className="timeline-brush"
            x={overlay.lo * 100}
            y={0}
            width={(overlay.hi - overlay.lo) * 100}
            height={100}
          />
        )}
      </svg>
      <div className="timeline-footer">
        {update.total_samples.toLocaleString()} samples · {durSec.toFixed(1)}s
        elapsed
        {range && (
          <>
            {" · "}
            <span className="timeline-range">
              brush {(Number(range.start_ns) / 1e9).toFixed(2)}s –{" "}
              {(Number(range.end_ns) / 1e9).toFixed(2)}s
            </span>{" "}
            <button
              className="timeline-clear"
              onClick={() => onRangeChange(null)}
              title="clear time-range filter"
            >
              clear
            </button>
          </>
        )}
      </div>
    </div>
  );
}
