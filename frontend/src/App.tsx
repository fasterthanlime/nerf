import { useEffect, useMemo, useRef, useState } from "react";
import { channel } from "@bearcove/vox-core";
import {
  LuStar,
  LuPackage,
  LuCircleHelp,
  LuBinary,
  LuSettings,
  LuPause,
  LuPlay,
  LuSun,
  LuMoon,
  LuChevronDown,
  LuCheck,
} from "react-icons/lu";
import { SiRust, SiC, SiCplusplus, SiSwift } from "react-icons/si";
import {
  connectProfiler,
  type AnnotatedView,
  type LiveFilter,
  type ProfilerClient,
  type SampleMode,
  type SymbolRef,
  type ThreadInfo,
  type ThreadsUpdate,
  type TopEntry,
  type TopSort,
  type TopUpdate,
  type ViewParams,
  type WakersUpdate,
} from "./generated/profiler.generated.ts";
import { Flamegraph } from "./Flamegraph.tsx";
import { Neighbors } from "./Neighbors.tsx";
import { Timeline } from "./Timeline.tsx";

type Status = "pending" | "ok" | "err";
type Theme = "dark" | "light";

/// Read the stored theme on first paint, falling back to the OS
/// preference. Wrapped in try/catch because some embedding contexts
/// throw on `localStorage` access.
function initialTheme(): Theme {
  try {
    const stored = localStorage.getItem("nperf-theme");
    if (stored === "light" || stored === "dark") return stored;
  } catch {}
  if (
    typeof window !== "undefined" &&
    window.matchMedia?.("(prefers-color-scheme: light)").matches
  ) {
    return "light";
  }
  return "dark";
}

export const EMPTY_FILTER: LiveFilter = {
  time_range: null,
  exclude_symbols: [],
  sample_mode: { tag: "Both" },
};

/// One step in the drill-down stack. Renders as a breadcrumb chip
/// above the flamegraph: clicking the body truncates the stack to
/// (and including) that step, clicking × removes just that one step.
///
/// `focus`   — narrows the flame to a subtree (absolute key in
///             `update.root`). Multiple focus steps nest.
/// `exclude` — drops every sample whose stack contains this symbol;
///             accumulates into `LiveFilter.exclude_symbols`.
export type DrillStep =
  | { kind: "focus"; absKey: string; label: string; binary: string | null }
  | { kind: "exclude"; symbol: SymbolRef; label: string };

/// Project a drill stack into the two derived view-state pieces:
/// the active flame focus key (the most recent focus step), and the
/// flat list of exclude symbols feeding into LiveFilter.
function deriveDrillView(stack: DrillStep[]): {
  currentAbsKey: string | null;
  excludeSymbols: SymbolRef[];
} {
  let currentAbsKey: string | null = null;
  const excludeSymbols: SymbolRef[] = [];
  for (const step of stack) {
    if (step.kind === "focus") currentAbsKey = step.absKey;
    else excludeSymbols.push(step.symbol);
  }
  return { currentAbsKey, excludeSymbols };
}

/// Bundle thread/filter knobs for a subscription. Centralising this so
/// every subscriber uses identical defaults; later we can thread a
/// filter object down from the UI (timeline brush, exclude pills, etc).
export function viewParams(
  tid: number | null,
  filter: LiveFilter = EMPTY_FILTER,
): ViewParams {
  return { tid, filter };
}

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
  const [tableFrozen, setTableFrozen] = useState(false);
  const [flameFrozen, setFlameFrozen] = useState(false);
  const frozen = tableFrozen || flameFrozen;
  const [sort, setSort] = useState<SortKey>("self");
  const [selectedTid, setSelectedTid] = useState<number | null>(null);
  const [threads, setThreads] = useState<ThreadInfo[]>([]);
  const [search, setSearch] = useState("");
  const [regexMode, setRegexMode] = useState(false);
  const [hiddenKinds, setHiddenKinds] = useState<Set<ObjKind>>(new Set());
  const [paneTab, setPaneTab] = useState<PaneTab>("asm");
  // Drill-down stack: each step is either a flame focus or a
  // symbol exclusion. Renders as breadcrumbs above the flamegraph.
  // F / right-click "Focus" / D / right-click "Drop" all push here;
  // clicking a breadcrumb truncates to that depth, the × on a
  // breadcrumb removes just that one step, Esc pops one level.
  const [drillStack, setDrillStack] = useState<DrillStep[]>([]);
  const [menu, setMenu] = useState<ContextMenuTarget | null>(null);
  // `filter` carries the non-stack filter knobs (time range, sample
  // mode). exclude_symbols is derived from drillStack and merged in
  // on the way to the wire so the two stay consistent.
  const [filter, setFilter] = useState<LiveFilter>(EMPTY_FILTER);
  const { currentAbsKey: flameFocusAbsKey, excludeSymbols: drillExcludes } =
    useMemo(() => deriveDrillView(drillStack), [drillStack]);
  const effectiveFilter: LiveFilter = useMemo(
    () => ({ ...filter, exclude_symbols: drillExcludes }),
    [filter, drillExcludes],
  );
  const [pmuMetric, setPmuMetric] = useState<PmuMetric>("ipc");
  const [theme, setTheme] = useState<Theme>(initialTheme);
  const [paused, setPaused] = useState(false);
  const [editingUrl, setEditingUrl] = useState(false);

  // Reflect the theme onto the <html> element so the CSS tokens flip,
  // and persist the user's choice across reloads.
  useEffect(() => {
    document.documentElement.dataset.theme = theme;
    try {
      localStorage.setItem("nperf-theme", theme);
    } catch {}
  }, [theme]);

  const dropSymbol = (s: { function_name: string | null; binary: string | null }) => {
    setDrillStack((prev) => {
      const alreadyExcluded = prev.some(
        (step) =>
          step.kind === "exclude" &&
          step.symbol.function_name === s.function_name &&
          step.symbol.binary === s.binary,
      );
      if (alreadyExcluded) return prev;
      return [
        ...prev,
        {
          kind: "exclude",
          symbol: { function_name: s.function_name, binary: s.binary },
          label: s.function_name ?? "(unresolved)",
        },
      ];
    });
  };

  const pushFocus = (step: { absKey: string; label: string; binary: string | null }) => {
    setDrillStack((prev) => [...prev, { kind: "focus", ...step }]);
  };

  const truncateDrill = (n: number) => setDrillStack((prev) => prev.slice(0, n));
  const removeDrillAt = (idx: number) =>
    setDrillStack((prev) => prev.filter((_, i) => i !== idx));
  const popDrill = () => setDrillStack((prev) => prev.slice(0, -1));

  const setTimeRange = (tr: LiveFilter["time_range"]) => {
    setFilter((prev) => ({ ...prev, time_range: tr }));
  };

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

  const openMenu = (target: ContextMenuTarget) => setMenu(target);

  // Compile a matcher once per (search, regexMode) change. Returns
  // `null` when the input is empty or regex compilation failed —
  // consumers treat null as "nothing matches". `regexError` holds the
  // compile error message for the UI to flag the input.
  const { matchText, regexError } = useMemo<{
    matchText: ((t: string) => boolean) | null;
    regexError: string | null;
  }>(() => {
    if (!search) return { matchText: null, regexError: null };
    if (regexMode) {
      try {
        const re = new RegExp(search, "i");
        return { matchText: (t: string) => re.test(t), regexError: null };
      } catch (err) {
        return { matchText: null, regexError: String(err) };
      }
    }
    const term = search.toLowerCase();
    return {
      matchText: (t: string) => t.toLowerCase().includes(term),
      regexError: null,
    };
  }, [search, regexMode]);
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
        console.debug("App: connecting to", committedUrl);
        const c = await connectProfiler(committedUrl);
        if (cancelled) return;
        console.debug("App: connected");
        setClient(c);
        setStatus("ok");

        const [tx, rx] = channel<TopUpdate>();
        const sortArg: TopSort =
          sort === "self" ? { tag: "BySelf" } : { tag: "ByTotal" };
        console.debug("App: subscribeTop", { sort: sortArg, tid: selectedTid });
        await c.subscribeTop(50, sortArg, viewParams(selectedTid, effectiveFilter), tx).catch((err) => {
          console.debug("App: subscribeTop call failed", err);
          if (!cancelled) {
            setStatus("err");
            setError(String(err));
          }
        });

        for await (const next of rx) {
          if (cancelled) break;
          console.debug(
            "App: top update",
            next.entries.length,
            "entries,",
            next.total_samples.toString(),
            "samples",
          );
          latest.current = next;
          // While frozen we accumulate into `latest` but don't render.
          // The mouse-leave handler will pull the freshest one.
          // Use a functional update so we can read the *current* frozen
          // value without it being a dep.
          setDisplayed((prev) => (tableFrozenRef.current ? prev : next));
        }
        console.debug("App: subscribeTop rx ended");
      } catch (err) {
        console.debug("App: subscribeTop loop threw", err);
        if (cancelled) return;
        setStatus("err");
        setError(String(err));
      }
    })();

    return () => {
      cancelled = true;
    };
  }, [committedUrl, sort, selectedTid, effectiveFilter]);

  // Subscribe to the live thread list whenever the client connects.
  useEffect(() => {
    if (!client) return;
    let cancelled = false;
    const [tx, rx] = channel<ThreadsUpdate>();
    client.subscribeThreads(tx).catch(() => {});
    (async () => {
      for await (const next of rx) {
        if (cancelled) break;
        setThreads(next.threads);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [client]);

  // "Who woke this thread?" — only meaningful when a single tid is
  // selected. Resubscribe on tid change; reset state on disconnect.
  const [wakers, setWakers] = useState<WakersUpdate | null>(null);
  useEffect(() => {
    setWakers(null);
    if (!client || selectedTid === null) return;
    let cancelled = false;
    const [tx, rx] = channel<WakersUpdate>();
    client.subscribeWakers(selectedTid, tx).catch(() => {});
    (async () => {
      for await (const next of rx) {
        if (cancelled) break;
        setWakers(next);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [client, selectedTid]);

  // Mirror table-pane frozen into a ref so the rx loop can check it
  // without re-running.
  const tableFrozenRef = useRef(tableFrozen);
  useEffect(() => {
    tableFrozenRef.current = tableFrozen;
    // When unfreezing, immediately apply whatever the latest snapshot is.
    if (!tableFrozen && latest.current) {
      setDisplayed(latest.current);
    }
  }, [tableFrozen]);

  return (
    <div className="shell">
      <header className="topbar">
        <div className="connection">
          <span className="status-slot">
            {frozen ? (
              <LuPause
                className="status-paused"
                title={`updates paused (hover the list to release)\nconnected to ${committedUrl}`}
              />
            ) : (
              <span
                className={`dot ${status}`}
                title={
                  status === "ok"
                    ? `connected to ${committedUrl} — double-click to change`
                    : `${status} · ${committedUrl}`
                }
                onDoubleClick={() => setEditingUrl(true)}
              />
            )}
          </span>
          {(status !== "ok" || editingUrl) && (
            <>
              <input
                value={url}
                onChange={(e) => setUrl(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") {
                    setCommittedUrl(url);
                    setEditingUrl(false);
                  }
                }}
                autoFocus={editingUrl}
                onBlur={() => {
                  if (status === "ok") setEditingUrl(false);
                }}
              />
              <button
                onClick={() => {
                  setCommittedUrl(url);
                  setEditingUrl(false);
                }}
              >
                connect
              </button>
            </>
          )}
          {error && <span className="err-text">{error}</span>}
          {client && (
            <button
              type="button"
              className={`pause-toggle${paused ? " active" : ""}`}
              onClick={() => {
                const next = !paused;
                setPaused(next);
                client.setPaused(next).catch(() => {});
              }}
              title={
                paused
                  ? "ingestion paused — click to resume sampling"
                  : "click to freeze the live view (target keeps running, no new samples flow)"
              }
            >
              {paused ? <LuPlay /> : <LuPause />}
              {paused ? "paused" : "pause"}
            </button>
          )}
          <ThreadSwitcher
            threads={threads}
            selectedTid={selectedTid}
            onSelect={setSelectedTid}
          />
          <span className="search-group">
            <input
              type="search"
              className={`search-input${regexError ? " err" : ""}`}
              placeholder={regexMode ? "regex" : "search symbols"}
              value={search}
              onChange={(e) => setSearch(e.target.value)}
              title={regexError ?? undefined}
            />
            <button
              type="button"
              className={`regex-toggle${regexMode ? " active" : ""}`}
              onClick={() => setRegexMode((m) => !m)}
              title={
                regexMode
                  ? "regex mode (case-insensitive); click to switch to substring"
                  : "substring mode; click to switch to regex"
              }
            >
              .*
            </button>
          </span>
          <SampleModeFilter
            mode={filter.sample_mode}
            onChange={(m) =>
              setFilter((prev) => ({ ...prev, sample_mode: m }))
            }
          />
          <PmuMetricFilter metric={pmuMetric} onChange={setPmuMetric} />
          <KindFilter hidden={hiddenKinds} onChange={setHiddenKinds} />
          <button
            type="button"
            className="theme-toggle"
            onClick={() => setTheme((t) => (t === "dark" ? "light" : "dark"))}
            title={`switch to ${theme === "dark" ? "light" : "dark"} mode`}
            aria-label="toggle color theme"
          >
            {theme === "dark" ? <LuSun /> : <LuMoon />}
          </button>
          <span className="spacer" />
          <span className="meta">
            {displayed
              ? `${displayed.total_samples.toLocaleString()} samples · ${displayed.entries.length} symbols`
              : "waiting for samples..."}
          </span>
        </div>
      </header>
      {client && (
        <section className="timeline-pane">
          <Timeline
            client={client}
            tid={selectedTid}
            range={filter.time_range}
            onRangeChange={setTimeRange}
          />
        </section>
      )}
      {drillStack.length > 0 && (
        <section className="drill-bar" aria-label="drill-down breadcrumbs">
          <button
            className="drill-crumb drill-root"
            onClick={() => truncateDrill(0)}
            title="clear all drill-down filters"
          >
            (all)
          </button>
          {drillStack.map((step, i) => (
            <span
              key={i}
              className={`drill-crumb drill-${step.kind}${i === drillStack.length - 1 ? " drill-current" : ""}`}
              title={
                step.kind === "exclude"
                  ? `excluded ${step.symbol.function_name ?? "(unresolved)"}`
                  : `focused ${step.label}`
              }
            >
              <button
                className="drill-crumb-body"
                onClick={() => truncateDrill(i + 1)}
                title="back to here (drops everything after)"
              >
                <span className="drill-kind">
                  {step.kind === "focus" ? "▸" : "−"}
                </span>
                <span className="drill-label">{step.label}</span>
              </button>
              <button
                className="drill-crumb-x"
                onClick={() => removeDrillAt(i)}
                title="remove just this step"
              >
                ×
              </button>
            </span>
          ))}
        </section>
      )}
      {wakers && wakers.entries.length > 0 && (
        <WakersPanel
          wakers={wakers}
          threads={threads}
          onSelectTid={setSelectedTid}
        />
      )}
      {client && (
        <section className="flame-pane">
          <Flamegraph
            client={client}
            tid={selectedTid}
            filter={effectiveFilter}
            matchText={matchText}
            hiddenKinds={hiddenKinds}
            currentAbsKey={flameFocusAbsKey}
            onPushFocus={pushFocus}
            onPopDrill={popDrill}
            onSelectAddress={setSelected}
            onFrozenChange={setFlameFrozen}
            onContextMenu={openMenu}
            onDropSymbol={dropSymbol}
          />
        </section>
      )}
      <main className="split">
        <section
          className={`pane top-pane${tableFrozen ? " frozen" : ""}`}
          onMouseEnter={() => setTableFrozen(true)}
          onMouseLeave={() => setTableFrozen(false)}
        >
          <TopTable
            entries={displayed?.entries ?? []}
            totalSamples={displayed?.total_samples ?? 0n}
            selected={selected}
            onSelect={setSelected}
            sort={sort}
            onSort={setSort}
            matchText={matchText}
            hiddenKinds={hiddenKinds}
            pmuMetric={pmuMetric}
            onContextMenu={openMenu}
          />
        </section>
        <section className="pane ann-pane">
          {client && selected !== null ? (
            <div className="ann-tabs" key={String(selected)}>
              <div className="tab-strip" role="tablist">
                <button
                  className={`tab${paneTab === "asm" ? " active" : ""}`}
                  onClick={() => setPaneTab("asm")}
                  role="tab"
                  aria-selected={paneTab === "asm"}
                >
                  disassembly
                </button>
                <button
                  className={`tab${paneTab === "neighbors" ? " active" : ""}`}
                  onClick={() => setPaneTab("neighbors")}
                  role="tab"
                  aria-selected={paneTab === "neighbors"}
                >
                  family tree
                </button>
              </div>
              <div className="tab-body">
                {paneTab === "asm" ? (
                  <Annotation
                    client={client}
                    address={selected}
                    tid={selectedTid}
                    filter={effectiveFilter}
                  />
                ) : (
                  <Neighbors
                    client={client}
                    address={selected}
                    tid={selectedTid}
                    filter={effectiveFilter}
                    matchText={matchText}
                    hiddenKinds={hiddenKinds}
                    onSelectAddress={setSelected}
                    onContextMenu={openMenu}
                  />
                )}
              </div>
            </div>
          ) : (
            <div className="placeholder">click a row to see disassembly</div>
          )}
        </section>
      </main>
      {menu && (
        <div
          className="context-menu"
          style={{ top: menu.y, left: menu.x }}
          onMouseDown={(e) => e.stopPropagation()}
          onClick={(e) => e.stopPropagation()}
        >
          <div className="context-menu-header">
            {menu.functionName ?? `0x${menu.address.toString(16)}`}
            {menu.binary && (
              <div className="context-menu-sub">{menu.binary}</div>
            )}
          </div>
          <button
            onClick={() => {
              setSelected(menu.address);
              setPaneTab("asm");
              setMenu(null);
            }}
          >
            Open disassembly
          </button>
          <button
            onClick={() => {
              setSelected(menu.address);
              setPaneTab("neighbors");
              setMenu(null);
            }}
          >
            Open family tree
          </button>
          {menu.flameKey && (
            <button
              onClick={() => {
                pushFocus({
                  absKey: menu.flameKey!,
                  label:
                    menu.functionName ??
                    `0x${menu.address.toString(16)}`,
                  binary: menu.binary,
                });
                setMenu(null);
              }}
            >
              Focus subtree in flamegraph
            </button>
          )}
          {menu.functionName && (
            <button
              onClick={() => {
                setSearch(`^${escapeRegex(menu.functionName!)}$`);
                setRegexMode(true);
                setMenu(null);
              }}
            >
              Search exact symbol
            </button>
          )}
          {menu.functionName && (
            <button
              onClick={() => {
                dropSymbol({
                  function_name: menu.functionName,
                  binary: menu.binary,
                });
                setMenu(null);
              }}
            >
              Drop samples with this symbol
            </button>
          )}
          {menu.kind && menu.kind !== "main" && (
            <button
              onClick={() => {
                const k = menu.kind!;
                setHiddenKinds((prev) => {
                  const next = new Set(prev);
                  next.add(k);
                  return next;
                });
                setMenu(null);
              }}
            >
              Hide all "{KIND_LABEL[menu.kind]}" rows
            </button>
          )}
          {menu.functionName && (
            <button
              onClick={() => {
                navigator.clipboard?.writeText(menu.functionName!);
                setMenu(null);
              }}
            >
              Copy symbol name
            </button>
          )}
        </div>
      )}
    </div>
  );
}

export type ContextMenuTarget = {
  x: number;
  y: number;
  address: bigint;
  functionName: string | null;
  binary: string | null;
  kind?: ObjKind;
  /// Only set when the source surface is a flamegraph; lets the menu
  /// offer "Focus subtree in flamegraph".
  flameKey?: string;
};

function escapeRegex(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

export type LangKind = "rust" | "c" | "cpp" | "swift" | "asm" | "unknown";
export type ObjKind = "main" | "system" | "dylib" | "unknown";
type PaneTab = "asm" | "neighbors";

/// Pick a language icon for a row. Prefers the server-side demangler
/// classification (carried on every TopEntry / FlameNode); only falls
/// back to a string heuristic when the demangler couldn't tell — for
/// instance unresolved hex addresses or images we never observed.
export function langOf(o: {
  function_name: string | null;
  language?: string;
}): LangKind {
  switch (o.language) {
    case "rust":
      return "rust";
    case "swift":
      return "swift";
    case "cpp":
    case "objcpp":
      return "cpp";
    case "objc":
    case "c":
      return "c";
  }
  const fn = o.function_name;
  if (!fn) return "unknown";
  if (fn.startsWith("0x")) return "asm";
  // Swift v5 mangling: `$s…` / `_$s…`. Pre-demangler fallback when
  // the symbol was never resolved.
  if (fn.startsWith("$s") || fn.startsWith("_$s") || fn.startsWith("_T"))
    return "swift";
  if (fn.includes("::")) return "rust";
  if (fn.includes("<") && fn.includes(">")) return "cpp";
  return "c";
}

/// Classify any object that has `is_main` + `binary` (TopEntry,
/// FlameNode) into a coarse kind we can color and filter by.
export function objKindOf(o: {
  is_main: boolean;
  binary: string | null;
}): ObjKind {
  if (o.is_main) return "main";
  const b = o.binary ?? "";
  if (!b) return "unknown";
  if (
    b.startsWith("libsystem_") ||
    b.startsWith("libobjc") ||
    b.startsWith("dyld") ||
    b.startsWith("libdyld") ||
    b.startsWith("libc++")
  ) {
    return "system";
  }
  return "dylib";
}

const KIND_LABEL: Record<ObjKind, string> = {
  main: "main",
  dylib: "dylib",
  system: "system",
  unknown: "other",
};

const KIND_ORDER: ObjKind[] = ["main", "dylib", "system", "unknown"];

function WakersPanel({
  wakers,
  threads,
  onSelectTid,
}: {
  wakers: WakersUpdate;
  threads: ThreadInfo[];
  onSelectTid: (tid: number) => void;
}) {
  const wakeeName = threads.find((t) => t.tid === wakers.wakee_tid)?.name;
  const totalNum = Number(wakers.total_wakeups);
  return (
    <section className="filter-bar wakers-bar">
      <span className="filter-bar-label">
        woken by · {wakeeName ?? `tid ${wakers.wakee_tid}`} ·{" "}
        {wakers.total_wakeups.toString()} wakeups
      </span>
      <div className="filter-chips wakers-chips">
        {wakers.entries.slice(0, 10).map((w, i) => {
          const fn =
            w.waker_function_name ?? `0x${w.waker_address.toString(16)}`;
          const wakerName = threads.find((t) => t.tid === w.waker_tid)?.name;
          const wakerLabel = wakerName ?? `tid ${w.waker_tid}`;
          const pct = totalNum > 0 ? (Number(w.count) / totalNum) * 100 : 0;
          return (
            <button
              key={i}
              className="wakers-chip"
              type="button"
              onClick={() => onSelectTid(w.waker_tid)}
              title={`click to switch to ${wakerLabel}${w.waker_binary ? " · " + w.waker_binary : ""}`}
            >
              <span className="wakers-chip-fn">{fn}</span>
              <span className="wakers-chip-tid">{wakerLabel}</span>
              <span className="wakers-chip-count">
                {w.count.toString()} ({pct.toFixed(0)}%)
              </span>
            </button>
          );
        })}
      </div>
    </section>
  );
}

function PmuMetricFilter({
  metric,
  onChange,
}: {
  metric: PmuMetric;
  onChange: (m: PmuMetric) => void;
}) {
  const options: { id: PmuMetric; label: string; title: string }[] = [
    {
      id: "ipc",
      label: "ipc",
      title: "instructions per cycle (fixed counters)",
    },
    {
      id: "l1d-miss",
      label: "l1d",
      title: "L1D cache misses per 1000 instructions (configurable counter)",
    },
    {
      id: "br-mispred",
      label: "br-miss",
      title: "branch mispredicts per 1000 instructions (configurable counter)",
    },
  ];
  return (
    <span className="kind-filter">
      {options.map((o) => {
        const active = metric === o.id;
        return (
          <button
            key={o.id}
            type="button"
            className={`kind-pill pmu-${o.id}${active ? "" : " off"}`}
            onClick={() => onChange(o.id)}
            title={o.title}
          >
            {o.label}
          </button>
        );
      })}
    </span>
  );
}

function SampleModeFilter({
  mode,
  onChange,
}: {
  mode: SampleMode;
  onChange: (m: SampleMode) => void;
}) {
  const options: { tag: "Both" | "OnCpu" | "OffCpu"; label: string; title: string }[] = [
    {
      tag: "Both",
      label: "wall",
      title: "wall-clock: on-CPU + off-CPU samples (default)",
    },
    {
      tag: "OnCpu",
      label: "on-cpu",
      title: "on-CPU only — what samply / Time Profiler show",
    },
    {
      tag: "OffCpu",
      label: "off-cpu",
      title: "off-CPU only — where threads are blocked",
    },
  ];
  return (
    <span className="kind-filter">
      {options.map((opt) => {
        const active = mode.tag === opt.tag;
        return (
          <button
            key={opt.tag}
            type="button"
            className={`kind-pill mode-${opt.tag.toLowerCase()}${active ? "" : " off"}`}
            onClick={() => onChange({ tag: opt.tag } as SampleMode)}
            title={opt.title}
          >
            {opt.label}
          </button>
        );
      })}
    </span>
  );
}

function KindFilter({
  hidden,
  onChange,
}: {
  hidden: Set<ObjKind>;
  onChange: (next: Set<ObjKind>) => void;
}) {
  return (
    <span className="kind-filter">
      {KIND_ORDER.map((k) => {
        const off = hidden.has(k);
        return (
          <button
            key={k}
            type="button"
            className={`kind-pill kind-${k}${off ? " off" : ""}`}
            onClick={() => {
              const next = new Set(hidden);
              if (off) next.delete(k);
              else next.add(k);
              onChange(next);
            }}
            title={
              off
                ? `${KIND_LABEL[k]} hidden — click to show`
                : `${KIND_LABEL[k]} shown — click to hide`
            }
          >
            {KIND_LABEL[k]}
          </button>
        );
      })}
    </span>
  );
}

export function langIcon(lang: LangKind) {
  switch (lang) {
    case "rust":
      return <SiRust title="Rust" />;
    case "c":
      return <SiC title="C" />;
    case "cpp":
      return <SiCplusplus title="C++" />;
    case "swift":
      return <SiSwift title="Swift" />;
    case "asm":
      return <LuBinary title="machine code" />;
    case "unknown":
      return <LuCircleHelp title="unknown" />;
  }
}

function barPct(count: bigint, total: bigint): string {
  if (total === 0n) return "0%";
  // 4 decimals of precision via integer math, then format.
  const ratio = Number((count * 10000n) / total) / 100;
  return `${Math.min(100, ratio)}%`;
}

/// Format a per-thousand-instructions ratio. Used for cache-miss
/// and branch-mispredict density: "this many cache misses per 1000
/// retired instructions" is the standard metric. Returns null when
/// the underlying counters didn't fire on this chip.
function rateLabel(
  events: bigint,
  insns: bigint,
  badAt: number,
  goodAt: number,
  unit: string,
  tooltip: string,
): React.ReactNode {
  if (insns === 0n) return null;
  if (events === 0n) return null;
  // events per kilo-insn = events / insns * 1000
  const rate = (Number(events) / Number(insns)) * 1000;
  // Higher = worse for misses; ramp red→green inverted.
  const t = Math.max(0, Math.min(1, (rate - goodAt) / (badAt - goodAt)));
  // 130 = green, 0 = red.
  const hue = Math.round(130 - t * 130);
  return (
    <div
      className="num-ipc"
      title={tooltip}
      style={{ color: `hsl(${hue} 75% 60%)` }}
    >
      {rate.toFixed(2)} {unit}
    </div>
  );
}

/// Render the IPC (instructions / cycles) for one TopEntry. Uses
/// the inclusive (total_*) counters because that's what the user
/// cares about for a row that aggregates a function plus its
/// callees. Returns null when the kperf backend didn't report PMU
/// values (Linux samples, off-CPU samples, samply runs).
function ipcLabel(e: TopEntry): React.ReactNode {
  const cycles = e.total_cycles;
  const insns = e.total_instructions;
  if (cycles === 0n) return null;
  // bigint -> number: typical sample totals are well within Number's
  // safe integer range; even a cap'd 100k samples × ~10M cycles per
  // sample is ~1e12, comfortable.
  const ipc = Number(insns) / Number(cycles);
  // Hue from red (poor) to green (excellent), pivoting around 1.0.
  // Clamp to 0.0..3.0 so off-the-chart values still get a colour.
  const t = Math.max(0, Math.min(1, (ipc - 0.5) / 2.0));
  const hue = Math.round(t * 130); // 0 = red, 130 ≈ green
  return (
    <div
      className="num-ipc"
      title={`${insns.toString()} insns / ${cycles.toString()} cycles`}
      style={{ color: `hsl(${hue} 75% 60%)` }}
    >
      {ipc.toFixed(2)} ipc
    </div>
  );
}

/// Render the active PMU metric for one TopEntry. The user can
/// switch between IPC (instructions per cycle), L1D miss rate, and
/// branch-mispredict rate via the topbar pill.
function pmuLabel(e: TopEntry, metric: PmuMetric): React.ReactNode {
  switch (metric) {
    case "ipc":
      return ipcLabel(e);
    case "l1d-miss":
      return rateLabel(
        e.total_l1d_misses,
        e.total_instructions,
        20,
        2,
        "miss/Kinsn",
        `${e.total_l1d_misses.toString()} L1D misses / ${e.total_instructions.toString()} insns`,
      );
    case "br-mispred":
      return rateLabel(
        e.total_branch_mispreds,
        e.total_instructions,
        10,
        0.5,
        "miss/Kinsn",
        `${e.total_branch_mispreds.toString()} branch mispreds / ${e.total_instructions.toString()} insns`,
      );
  }
}

type PmuMetric = "ipc" | "l1d-miss" | "br-mispred";

function objIcon(obj: ObjKind) {
  switch (obj) {
    case "main":
      return <LuStar title="main executable" />;
    case "system":
      return <LuSettings title="system library" />;
    case "dylib":
      return <LuPackage title="dynamic library" />;
    case "unknown":
      return <LuCircleHelp title="unmapped (JIT or kernel)" />;
  }
}

function entryMatches(
  e: TopEntry,
  matchText: ((t: string) => boolean) | null,
): boolean {
  if (!matchText) return false;
  return (
    (e.function_name != null && matchText(e.function_name)) ||
    (e.binary != null && matchText(e.binary))
  );
}

function TopTable({
  entries,
  totalSamples,
  selected,
  onSelect,
  sort,
  onSort,
  matchText,
  hiddenKinds,
  pmuMetric,
  onContextMenu,
}: {
  entries: TopEntry[];
  totalSamples: bigint;
  selected: bigint | null;
  onSelect: (a: bigint) => void;
  sort: SortKey;
  onSort: (s: SortKey) => void;
  matchText: ((t: string) => boolean) | null;
  hiddenKinds: Set<ObjKind>;
  pmuMetric: PmuMetric;
  onContextMenu: (t: ContextMenuTarget) => void;
}) {
  const visible = entries.filter((e) => !hiddenKinds.has(objKindOf(e)));
  // Scale every row's progress bar against the busiest visible row,
  // not the recording's grand total. With 1M+ samples spread across
  // hundreds of symbols, the grand-total denominator made every bar
  // a hairline -- now the leader fills the bar and the rest are
  // proportional to it (the "Activity Monitor" model).
  const barDenom = visible.reduce((m, e) => {
    const v = sort === "self" ? e.self_count : e.total_count;
    return v > m ? v : m;
  }, 1n);
  return (
    <table className="top-table">
      <thead>
        <tr>
          <th>function · binary</th>
          <th className="num-h">
            <span
              className={`sortable${sort === "self" ? " active" : ""}`}
              onClick={() => onSort("self")}
            >
              self{sort === "self" ? " ↓" : ""}
            </span>
            <span className="sep"> / </span>
            <span
              className={`sortable${sort === "total" ? " active" : ""}`}
              onClick={() => onSort("total")}
            >
              total{sort === "total" ? " ↓" : ""}
            </span>
          </th>
        </tr>
      </thead>
      <tbody>
        {visible.map((e) => {
          const lang = langOf(e);
          const obj = objKindOf(e);
          const fnLabel = e.function_name ?? `0x${e.address.toString(16)}`;
          const binLabel = e.binary ?? "(no binary)";
          return (
            <tr
              key={String(e.address)}
              className={
                (selected === e.address ? "selected " : "") +
                (e.is_main ? "main " : "") +
                (entryMatches(e, matchText) ? "match" : "")
              }
              onClick={() => onSelect(e.address)}
              onContextMenu={(ev) => {
                ev.preventDefault();
                onContextMenu({
                  x: ev.clientX,
                  y: ev.clientY,
                  address: e.address,
                  functionName: e.function_name,
                  binary: e.binary,
                  kind: obj,
                });
              }}
            >
              <td className="entry">
                <div className="entry-line fn-line">
                  <span className={`glyph lang-${lang}`}>
                    {langIcon(lang)}
                  </span>
                  <span className="fn-name">{fnLabel}</span>
                </div>
                <div className="entry-line bin-line">
                  <span className={`glyph obj-${obj}`}>{objIcon(obj)}</span>
                  <span className="bin-name">{binLabel}</span>
                </div>
              </td>
              <td className="num">
                <div className="num-line">
                  {e.self_count === e.total_count ? (
                    e.self_count.toString()
                  ) : (
                    <>
                      {e.self_count.toString()}
                      <span className="num-sep"> / </span>
                      <span className="num-total">
                        {e.total_count.toString()}
                      </span>
                    </>
                  )}
                </div>
                <div className="num-bar">
                  <div
                    className="num-bar-fill"
                    style={{
                      width: barPct(
                        sort === "self" ? e.self_count : e.total_count,
                        barDenom,
                      ),
                    }}
                  />
                </div>
                <div className="num-ipc-slot">{pmuLabel(e, pmuMetric)}</div>
              </td>
            </tr>
          );
        })}
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

function threadLabel(t: ThreadInfo): string {
  return t.name ? `${t.name} [${t.tid}]` : `[${t.tid}]`;
}

/// Custom dropdown for filtering by thread. Replaces a `<select>` so
/// we can do live search (over name + tid), show per-row sample bars,
/// and later sort by other metrics (off-CPU samples, allocations…).
/// Threads arrive busiest-first from the server.
function ThreadSwitcher({
  threads,
  selectedTid,
  onSelect,
}: {
  threads: ThreadInfo[];
  selectedTid: number | null;
  onSelect: (tid: number | null) => void;
}) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const rootRef = useRef<HTMLDivElement | null>(null);
  const inputRef = useRef<HTMLInputElement | null>(null);

  // Close on outside click / Escape.
  useEffect(() => {
    if (!open) return;
    const onMouse = (e: MouseEvent) => {
      if (!rootRef.current?.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    window.addEventListener("mousedown", onMouse);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("mousedown", onMouse);
      window.removeEventListener("keydown", onKey);
    };
  }, [open]);

  // Auto-focus the search input when opening.
  useEffect(() => {
    if (open) {
      setQuery("");
      requestAnimationFrame(() => inputRef.current?.focus());
    }
  }, [open]);

  const total = threads.reduce((s, t) => s + t.sample_count, 0n);
  const totalF = total === 0n ? 1 : Number(total);
  const max = threads.reduce(
    (m, t) => (t.sample_count > m ? t.sample_count : m),
    0n,
  );
  const maxF = max === 0n ? 1 : Number(max);

  const q = query.trim().toLowerCase();
  const filtered = q
    ? threads.filter((t) => {
        if (String(t.tid).includes(q)) return true;
        if (t.name && t.name.toLowerCase().includes(q)) return true;
        return false;
      })
    : threads;

  const triggerLabel = (() => {
    if (selectedTid === null) return "all threads";
    const t = threads.find((x) => x.tid === selectedTid);
    return t ? threadLabel(t) : `[${selectedTid}]`;
  })();

  const pick = (tid: number | null) => {
    onSelect(tid);
    setOpen(false);
  };

  return (
    <div
      ref={rootRef}
      className={`thread-switcher${open ? " open" : ""}`}
    >
      <button
        type="button"
        className="thread-trigger"
        onClick={() => setOpen((o) => !o)}
        title="filter by thread"
      >
        <span className="thread-trigger-label">{triggerLabel}</span>
        <LuChevronDown className="thread-trigger-chev" />
      </button>
      {open && (
        <div className="thread-popover" role="listbox">
          <input
            ref={inputRef}
            className="thread-search"
            type="search"
            placeholder="search threads…"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && filtered.length === 1) {
                pick(filtered[0].tid);
              }
            }}
          />
          <div className="thread-list">
            <button
              type="button"
              className={`thread-row${selectedTid === null ? " selected" : ""}`}
              onClick={() => pick(null)}
            >
              <span className="thread-check">
                {selectedTid === null && <LuCheck />}
              </span>
              <span className="thread-name">all threads</span>
              <span className="thread-count">
                {total.toLocaleString()}
              </span>
            </button>
            {filtered.length === 0 ? (
              <div className="thread-empty">no matches</div>
            ) : (
              filtered.map((t) => {
                const sel = selectedTid === t.tid;
                const wPct =
                  max === 0n ? 0 : (Number(t.sample_count) / maxF) * 100;
                const rPct =
                  total === 0n
                    ? 0
                    : Math.round((Number(t.sample_count) / totalF) * 1000) /
                      10;
                return (
                  <button
                    type="button"
                    key={t.tid}
                    className={`thread-row${sel ? " selected" : ""}`}
                    onClick={() => pick(t.tid)}
                    title={`${t.sample_count.toString()} samples (${rPct}%)`}
                  >
                    <span className="thread-check">{sel && <LuCheck />}</span>
                    <span className="thread-name">
                      {t.name ?? <em className="thread-name-anon">[{t.tid}]</em>}
                      {t.name && (
                        <span className="thread-tid"> [{t.tid}]</span>
                      )}
                    </span>
                    <span className="thread-bar">
                      <span
                        className="thread-bar-fill"
                        style={{ width: `${wPct}%` }}
                      />
                    </span>
                    <span className="thread-count">
                      {t.sample_count.toLocaleString()}
                    </span>
                  </button>
                );
              })
            )}
          </div>
          <div className="thread-popover-footer">
            {threads.length} threads · sorted by samples
          </div>
        </div>
      )}
    </div>
  );
}

function Annotation({
  client,
  address,
  tid,
  filter,
}: {
  client: ProfilerClient;
  address: bigint;
  tid: number | null;
  filter: LiveFilter;
}) {
  const [view, setView] = useState<AnnotatedView | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const bodyRef = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    let cancelled = false;
    setView(null);
    setErr(null);

    const [tx, rx] = channel<AnnotatedView>();
    client.subscribeAnnotated(address, viewParams(tid, filter), tx).catch((e) => {
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
  }, [client, address, tid, filter]);

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
