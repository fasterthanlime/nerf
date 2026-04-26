use std::collections::HashMap;

use nperf_live_proto::TopEntry;

#[derive(Clone, Copy)]
pub struct RawTopEntry {
    pub address: u64,
    pub self_count: u64,
    pub total_count: u64,
}

#[derive(Default)]
pub struct Aggregator {
    /// Self-count: the leaf frame of each sample.
    self_counts: HashMap<u64, u64>,
    /// Total-count: any time an address appears anywhere in the stack.
    total_counts: HashMap<u64, u64>,
    total_samples: u64,
    /// Call tree built from full stacks. Walked root-first so children
    /// represent callees of their parent. The root itself is never
    /// incremented; total_samples is the sum of root.children.count.
    pub(crate) flame_root: StackNode,
}

/// One node in the call tree. Children keyed by callee address.
#[derive(Default)]
pub(crate) struct StackNode {
    pub(crate) count: u64,
    pub(crate) children: HashMap<u64, StackNode>,
}

impl Aggregator {
    pub fn total_samples(&self) -> u64 {
        self.total_samples
    }

    pub fn self_count(&self, address: u64) -> u64 {
        self.self_counts.get(&address).copied().unwrap_or(0)
    }

    pub fn record(&mut self, user_addrs: &[u64]) {
        self.total_samples += 1;
        if let Some(&leaf) = user_addrs.first() {
            *self.self_counts.entry(leaf).or_insert(0) += 1;
        }

        // Each address contributes once per sample to total, even if it
        // appears multiple times (recursion).
        let mut seen: smallset::SmallSet = Default::default();
        for &addr in user_addrs {
            if seen.insert(addr) {
                *self.total_counts.entry(addr).or_insert(0) += 1;
            }
        }

        // Build the call tree. user_addrs is leaf-first, so iterate
        // reversed to walk root → leaf.
        let mut node = &mut self.flame_root;
        for &addr in user_addrs.iter().rev() {
            node = node.children.entry(addr).or_default();
            node.count += 1;
        }
    }

    pub fn top(&self, limit: usize) -> Vec<TopEntry> {
        self.top_raw(limit)
            .into_iter()
            .map(|e| TopEntry {
                address: e.address,
                self_count: e.self_count,
                total_count: e.total_count,
                function_name: None,
                binary: None,
                is_main: false,
            })
            .collect()
    }

    /// Top-N as raw addresses + counts, for callers (the live server)
    /// that want to layer symbol resolution on top.
    ///
    /// Iterates the *total*-counts universe rather than the self-counts
    /// universe so symbols that only ever appear as inner frames (e.g.
    /// `drop_in_place<T>` in a tower of `_xzm_free` calls) still show up.
    pub fn top_raw(&self, limit: usize) -> Vec<RawTopEntry> {
        let mut entries: Vec<RawTopEntry> = self
            .total_counts
            .iter()
            .map(|(&address, &total_count)| RawTopEntry {
                address,
                self_count: self.self_counts.get(&address).copied().unwrap_or(0),
                total_count,
            })
            .collect();
        // Primary sort by self (where time is actually being spent at the
        // leaf), tiebreak by total (where the function appears on the
        // stack at all), so pure inner frames don't all collapse to the
        // bottom in arbitrary order.
        entries.sort_by(|a, b| {
            b.self_count
                .cmp(&a.self_count)
                .then_with(|| b.total_count.cmp(&a.total_count))
        });
        entries.truncate(limit);
        entries
    }
}

mod smallset {
    /// Tiny set optimised for typical stack depths (<32). Linear search; no allocs
    /// unless a stack is huge.
    #[derive(Default)]
    pub struct SmallSet {
        items: Vec<u64>,
    }

    impl SmallSet {
        pub fn insert(&mut self, value: u64) -> bool {
            if self.items.contains(&value) {
                false
            } else {
                self.items.push(value);
                true
            }
        }
    }
}
