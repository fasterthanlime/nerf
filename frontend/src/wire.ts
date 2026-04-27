// Hydration layer between the on-the-wire string-table-encoded
// FlameNode (function_name / binary / language are u32 indices into
// FlamegraphUpdate.strings) and what the rest of the frontend wants
// to see (those fields as inline strings).
//
// The tree is rebuilt once per snapshot. Identical indices share
// references so V8 only holds one copy of each interned string; the
// FlameView tree's footprint is roughly the same as the equivalent
// string-inline tree, with the wire savings showing up purely on the
// network.

import type {
  FlameNode as WireFlameNode,
  FlamegraphUpdate as WireFlamegraphUpdate,
  NeighborsUpdate as WireNeighborsUpdate,
} from "./generated/profiler.generated.ts";

export interface FlameView {
  address: bigint;
  /// Wall-clock time spent at (or under) this node, in nanoseconds.
  duration_ns: bigint;
  function_name: string | null;
  binary: string | null;
  is_main: boolean;
  language: string;
  cycles: bigint;
  instructions: bigint;
  l1d_misses: bigint;
  branch_mispreds: bigint;
  children: FlameView[];
}

export interface FlamegraphView {
  total_duration_ns: bigint;
  root: FlameView;
}

export interface NeighborsView {
  function_name: string | null;
  binary: string | null;
  is_main: boolean;
  language: string;
  own_duration_ns: bigint;
  callers_tree: FlameView;
  callees_tree: FlameView;
}

function lookup(strings: string[], idx: number | null): string | null {
  return idx == null ? null : strings[idx];
}

function hydrateNode(node: WireFlameNode, strings: string[]): FlameView {
  return {
    address: node.address,
    duration_ns: node.duration_ns,
    function_name: lookup(strings, node.function_name),
    binary: lookup(strings, node.binary),
    is_main: node.is_main,
    language: strings[node.language],
    cycles: node.cycles,
    instructions: node.instructions,
    l1d_misses: node.l1d_misses,
    branch_mispreds: node.branch_mispreds,
    children: node.children.map((c) => hydrateNode(c, strings)),
  };
}

export function hydrateFlamegraph(u: WireFlamegraphUpdate): FlamegraphView {
  return {
    total_duration_ns: u.total_duration_ns,
    root: hydrateNode(u.root, u.strings),
  };
}

export function hydrateNeighbors(u: WireNeighborsUpdate): NeighborsView {
  return {
    function_name: lookup(u.strings, u.function_name),
    binary: lookup(u.strings, u.binary),
    is_main: u.is_main,
    language: u.strings[u.language],
    own_duration_ns: u.own_duration_ns,
    callers_tree: hydrateNode(u.callers_tree, u.strings),
    callees_tree: hydrateNode(u.callees_tree, u.strings),
  };
}

/// Format a nanosecond duration as a human-readable string. Used
/// across the UI now that the aggregator's unit is wall-clock time.
export function formatDuration(ns: bigint): string {
  if (ns === 0n) return "0";
  const n = Number(ns);
  if (n < 1_000) return `${n}ns`;
  if (n < 1_000_000) return `${(n / 1_000).toFixed(1)}µs`;
  if (n < 1_000_000_000) return `${(n / 1_000_000).toFixed(1)}ms`;
  if (n < 60_000_000_000) return `${(n / 1_000_000_000).toFixed(2)}s`;
  const minutes = Math.floor(n / 60_000_000_000);
  const seconds = (n % 60_000_000_000) / 1_000_000_000;
  return `${minutes}m${seconds.toFixed(1)}s`;
}
