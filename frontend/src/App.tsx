import { useEffect, useRef, useState } from "react";
import { channel } from "@bearcove/vox-core";
import {
  connectProfiler,
  type AnnotatedView,
  type ProfilerClient,
  type TopEntry,
  type TopUpdate,
} from "./generated/profiler.generated.ts";

type Status = "pending" | "ok" | "err";

function defaultUrl(): string {
  const params = new URLSearchParams(window.location.search);
  return params.get("ws") ?? "ws://127.0.0.1:8080";
}

export function App() {
  const [url, setUrl] = useState(defaultUrl());
  const [committedUrl, setCommittedUrl] = useState(url);
  const [status, setStatus] = useState<Status>("pending");
  const [error, setError] = useState<string | null>(null);
  const [client, setClient] = useState<ProfilerClient | null>(null);
  const [displayed, setDisplayed] = useState<TopUpdate | null>(null);
  const [selected, setSelected] = useState<bigint | null>(null);
  const [frozen, setFrozen] = useState(false);
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
        c.subscribeTop(50, tx).catch((err) => {
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
  }, [committedUrl]);

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
}: {
  entries: TopEntry[];
  selected: bigint | null;
  onSelect: (a: bigint) => void;
}) {
  return (
    <table className="top-table">
      <thead>
        <tr>
          <th>function</th>
          <th>binary</th>
          <th className="num-h">self</th>
          <th className="num-h">total</th>
        </tr>
      </thead>
      <tbody>
        {entries.map((e) => (
          <tr
            key={String(e.address)}
            className={selected === e.address ? "selected" : ""}
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

function Annotation({
  client,
  address,
}: {
  client: ProfilerClient;
  address: bigint;
}) {
  const [view, setView] = useState<AnnotatedView | null>(null);
  const [err, setErr] = useState<string | null>(null);

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

  return (
    <div className="annotation">
      <div className="ann-header">
        {view ? view.function_name : "loading..."}
        {err && <span className="err-text"> · {err}</span>}
      </div>
      <div className="ann-body">
        <table className="asm">
          <tbody>
            {view?.lines.map((line) => (
              <tr key={String(line.address)}>
                <td className="num">
                  {line.self_count > 0n ? line.self_count.toString() : ""}
                </td>
                <td className="addr">0x{line.address.toString(16)}</td>
                <td
                  className="asm-line"
                  dangerouslySetInnerHTML={{ __html: line.html }}
                />
              </tr>
            )) ?? null}
          </tbody>
        </table>
      </div>
    </div>
  );
}
