import { useEffect, useState } from "react";
import { channel } from "@bearcove/vox-core";
import type {
  ProfilerClient,
  TimelineUpdate,
} from "./generated/profiler.generated.ts";

/// Compact timeline strip across the top of the page. Each bucket is
/// drawn as a vertical bar; bar height is proportional to the bucket's
/// sample count relative to the busiest bucket in view.
export function Timeline({
  client,
  tid,
}: {
  client: ProfilerClient;
  tid: number | null;
}) {
  const [update, setUpdate] = useState<TimelineUpdate | null>(null);

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

  return (
    <div className="timeline">
      <div className="timeline-bars">
        {update.buckets.map((b, i) => {
          const h = max === 0n ? 0 : Math.round((Number(b.count) / maxF) * 100);
          return (
            <div
              key={i}
              className="timeline-bar"
              style={{ height: `${h}%` }}
              title={`${(Number(b.start_ns) / 1e9).toFixed(2)}s · ${b.count.toString()} samples`}
            />
          );
        })}
      </div>
      <div className="timeline-footer">
        {update.total_samples.toLocaleString()} samples · {durSec.toFixed(1)}s
        elapsed · bucket {(Number(update.bucket_size_ns) / 1e6).toFixed(0)}ms
      </div>
    </div>
  );
}
