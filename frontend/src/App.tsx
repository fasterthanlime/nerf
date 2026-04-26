import { useEffect, useRef, useState } from "react";
import { channel } from "@bearcove/vox-core";
import {
  connectProfiler,
  type AnnotatedView,
  type ProfilerClient,
  type TopEntry,
  type TopSort,
  type TopUpdate,
} from "./generated/profiler.generated.ts";

type Status = "pending" | "ok" | "err";

function defaultUrl(): string {
  const params = new URLSearchParams(window.location.search);
  return params.get("ws") ?? "ws://127.0.0.1:8080";
}

type SortKey = "self" | "total";

export function App() {
  const [url, setUrl] = useState(defaultUrl());
  const [committedUrl, setCommittedUrl] = useState(url);
  const [status, setStatus] = useState<Status>("pending");
  const [error, setError] = useState<string | null>(null);
  const [client, setClient] = useState<ProfilerClient | null>(null);
  const [displayed, setDisplayed] = useState<TopUpdate | null>(null);
  const [selected, setSelected] = useState<bigint | null>(null);
  const [frozen, setFrozen] = useState(false);
  const [sort, setSort] = useState<SortKey>("self");
  // Latest update kept in a ref so the frozen-gate logic can pull the
  // most recent snapshot when the mouse leaves without re-running the
  // subscribe effect.
  const latest = useRef<TopUpdate | null>(null);

  useEffect(() => {
    let cancelled = false;
    setStatus("pending");
    setError(null);
    latest.current = null;
    setDisplayed(null);
    setClient(null);
    setSelected(null);

    (async () => {
      try {
        const c = await connectProfiler(committedUrl);
        if (cancelled) return;
        setClient(c);
        setStatus("ok");

        const [tx, rx] = channel<TopUpdate>();
        const sortArg: TopSort =
          sort === "self" ? { tag: "BySelf" } : { tag: "ByTotal" };
        c.subscribeTop(50, sortArg, tx).catch((err) => {
          if (!cancelled) {
            setStatus("err");
            setError(String(err));
          }
        });

        for await (const next of rx) {
          if (cancelled) break;
          latest.current = next;
          // While frozen we accumulate into `latest` but don't render.
          // The mouse-leave handler will pull the freshest one.
          // Use a functional update so we can read the *current* frozen
          // value without it being a dep.
          setDisplayed((prev) => (frozenRef.current ? prev : next));
        }
      } catch (err) {
        if (cancelled) return;
        setStatus("err");
        setError(String(err));
      }
    })();

    return () => {
      cancelled = true;
    };
  }, [committedUrl, sort]);

  // Mirror `frozen` into a ref so the rx loop can check it without re-running.
  const frozenRef = useRef(frozen);
  useEffect(() => {
    frozenRef.current = frozen;
    // When unfreezing, immediately apply whatever the latest snapshot is.
    if (!frozen && latest.current) {
      setDisplayed(latest.current);
    }
  }, [frozen]);

  return (
    <div className="shell">
      <header className="topbar">
        <h1>nperf live</h1>
        <div className="connection">
          <span className={`dot ${status}`} />
          <input
            value={url}
            onChange={(e) => setUrl(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") setCommittedUrl(url);
            }}
          />
          <button onClick={() => setCommittedUrl(url)}>connect</button>
          {error && <span className="err-text">{error}</span>}
          <span className="spacer" />
          <span className="meta">
            {displayed
              ? `${displayed.total_samples.toLocaleString()} samples · ${displayed.entries.length} symbols`
              : "waiting for samples..."}
          </span>
        </div>
      </header>
      <main className="split">
        <section
          className={`pane top-pane${frozen ? " frozen" : ""}`}
          onMouseEnter={() => setFrozen(true)}
          onMouseLeave={() => setFrozen(false)}
        >
          {frozen && <div className="frozen-badge">paused (hover)</div>}
          <TopTable
            entries={displayed?.entries ?? []}
            selected={selected}
            onSelect={setSelected}
            sort={sort}
            onSort={setSort}
          />
        </section>
        <section className="pane ann-pane">
          {client && selected !== null ? (
            <Annotation client={client} address={selected} key={String(selected)} />
          ) : (
            <div className="placeholder">click a row to see disassembly</div>
          )}
        </section>
      </main>
    </div>
  );
}

function TopTable({
  entries,
  selected,
  onSelect,
  sort,
  onSort,
}: {
  entries: TopEntry[];
  selected: bigint | null;
  onSelect: (a: bigint) => void;
  sort: SortKey;
  onSort: (s: SortKey) => void;
}) {
  return (
    <table className="top-table">
      <thead>
        <tr>
          <th>function</th>
          <th>binary</th>
          <th
            className={`num-h sortable${sort === "self" ? " active" : ""}`}
            onClick={() => onSort("self")}
          >
            self{sort === "self" ? " ↓" : ""}
          </th>
          <th
            className={`num-h sortable${sort === "total" ? " active" : ""}`}
            onClick={() => onSort("total")}
          >
            total{sort === "total" ? " ↓" : ""}
          </th>
        </tr>
      </thead>
      <tbody>
        {entries.map((e) => (
          <tr
            key={String(e.address)}
            className={
              (selected === e.address ? "selected " : "") +
              (e.is_main ? "main" : "")
            }
            onClick={() => onSelect(e.address)}
          >
            <td className="fn">
              {e.function_name ?? (
                <span className="addr">0x{e.address.toString(16)}</span>
              )}
            </td>
            <td className="bin">{e.binary ?? ""}</td>
            <td className="num">{e.self_count.toString()}</td>
            <td className="num">{e.total_count.toString()}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

/// Map a sample count to a heat color. 0 → transparent; otherwise
/// interpolates blue → yellow → red based on relative intensity, with
/// alpha rising for hotter lines so the eye picks them out.
function heatBg(count: bigint, max: bigint): string {
  if (count === 0n || max === 0n) return "transparent";
  const t = Math.max(0, Math.min(1, Number(count) / Number(max)));
  let hue: number;
  if (t < 0.5) {
    // blue (240°) → yellow (60°)
    hue = 240 - (240 - 60) * (t * 2);
  } else {
    // yellow (60°) → red (0°)
    hue = 60 - 60 * ((t - 0.5) * 2);
  }
  const alpha = 0.15 + 0.5 * t;
  return `hsla(${hue}, 70%, 45%, ${alpha})`;
}

function Annotation({
  client,
  address,
}: {
  client: ProfilerClient;
  address: bigint;
}) {
  const [view, setView] = useState<AnnotatedView | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const bodyRef = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    let cancelled = false;
    setView(null);
    setErr(null);

    const [tx, rx] = channel<AnnotatedView>();
    client.subscribeAnnotated(address, tx).catch((e) => {
      if (!cancelled) setErr(String(e));
    });

    (async () => {
      for await (const next of rx) {
        if (cancelled) break;
        setView(next);
      }
    })();

    return () => {
      cancelled = true;
    };
  }, [client, address]);

  const lines = view?.lines ?? [];
  const maxSelf = lines.reduce(
    (m, l) => (l.self_count > m ? l.self_count : m),
    0n,
  );

  const jumpTo = (addr: bigint) => {
    const tr = bodyRef.current?.querySelector(
      `tr[data-addr="${String(addr)}"]`,
    ) as HTMLElement | null;
    tr?.scrollIntoView({ block: "center" });
  };

  return (
    <div className="annotation">
      <div className="ann-header">
        {view ? view.function_name : "loading..."}
        {err && <span className="err-text"> · {err}</span>}
      </div>
      <div className="ann-content">
        <div className="ann-body" ref={bodyRef}>
          <table className="asm">
            <tbody>
              {lines.flatMap((line) => {
                const off = line.address - view!.base_address;
                const sh = line.source_header;
                const rows = [];
                if (sh) {
                  // Banner row above the asm rows for this source line.
                  // file:line on the left, highlighted snippet on the right.
                  const basename = sh.file.split("/").pop() ?? sh.file;
                  rows.push(
                    <tr
                      key={`src-${String(line.address)}`}
                      className="src-header"
                    >
                      <td className="src-loc" colSpan={2}>
                        {basename}:{sh.line}
                      </td>
                      <td
                        className="src-snip"
                        dangerouslySetInnerHTML={{
                          __html:
                            sh.html.length > 0
                              ? sh.html
                              : "(source not on disk)",
                        }}
                      />
                    </tr>,
                  );
                }
                rows.push(
                  <tr
                    key={String(line.address)}
                    data-addr={String(line.address)}
                    style={{ background: heatBg(line.self_count, maxSelf) }}
                  >
                    <td className="num">
                      {line.self_count > 0n ? line.self_count.toString() : ""}
                    </td>
                    <td className="addr">+0x{off.toString(16)}</td>
                    <td
                      className="asm-line"
                      dangerouslySetInnerHTML={{ __html: line.html }}
                    />
                  </tr>,
                );
                return rows;
              })}
            </tbody>
          </table>
        </div>
        {lines.length > 0 && (
          <div className="ann-minimap" aria-label="heatmap minimap">
            {lines.map((line) => (
              <div
                key={String(line.address)}
                className="mm-row"
                title={`+0x${(line.address - view!.base_address).toString(16)} · ${line.self_count.toString()} samples`}
                style={{ background: heatBg(line.self_count, maxSelf) }}
                onClick={() => jumpTo(line.address)}
              />
            ))}
          </div>
        )}
      </div>
    </div>
  );
}
