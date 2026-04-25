use std::collections::HashMap;

use crate::TopEntry;

#[derive(Default)]
pub struct Aggregator {
    /// Self-count: the leaf frame of each sample.
    self_counts: HashMap<u64, u64>,
    /// Total-count: any time an address appears anywhere in the stack.
    total_counts: HashMap<u64, u64>,
    total_samples: u64,
}

impl Aggregator {
    pub fn total_samples(&self) -> u64 {
        self.total_samples
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
    }

    pub fn top(&self, limit: usize) -> Vec<TopEntry> {
        let mut entries: Vec<TopEntry> = self
            .self_counts
            .iter()
            .map(|(&address, &self_count)| TopEntry {
                address,
                self_count,
                total_count: self.total_counts.get(&address).copied().unwrap_or(0),
            })
            .collect();
        entries.sort_by(|a, b| b.self_count.cmp(&a.self_count));
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
