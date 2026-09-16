// Copyright 2026 Adrian Mârza and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Deterministic LRU admission for materialized operation prefixes.

use std::collections::VecDeque;

pub(super) const PREFIX_MEMORY_BUDGET: u64 = 512 * 1024 * 1024;

pub(super) struct Policy {
    budget: u64,
    bytes: u64,
    order: VecDeque<(String, u64)>,
}

impl Policy {
    pub(super) fn new(budget: u64) -> Self {
        Self {
            budget,
            bytes: 0,
            order: VecDeque::new(),
        }
    }

    pub(super) fn touch(&mut self, key: &str) {
        if let Some(index) = self.order.iter().position(|(item, _)| item == key) {
            let entry = self.order.remove(index).unwrap();
            self.order.push_back(entry);
        }
    }

    pub(super) fn admit(&mut self, key: String, bytes: u64) -> (bool, Vec<String>) {
        // Oversized states remain usable by the current leaf but do not
        // displace the cache. The immutable root is managed separately.
        if bytes > self.budget {
            return (false, Vec::new());
        }
        let mut evicted = Vec::new();
        while self.bytes.saturating_add(bytes) > self.budget {
            let (oldest, weight) = self.order.pop_front().unwrap();
            self.bytes -= weight;
            evicted.push(oldest);
        }
        self.order.push_back((key, bytes));
        self.bytes += bytes;
        (true, evicted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eviction_is_lru_and_oversized_prefixes_never_enter_the_cache() {
        let mut cache = Policy::new(10);
        assert_eq!(cache.admit("a".into(), 4), (true, vec![]));
        assert_eq!(cache.admit("b".into(), 5), (true, vec![]));
        cache.touch("a");
        assert_eq!(cache.admit("c".into(), 6), (true, vec!["b".into()]));
        assert_eq!(cache.bytes, 10);
        assert_eq!(cache.admit("large".into(), 11), (false, vec![]));
        assert_eq!(cache.bytes, 10);
        assert_eq!(
            cache.admit("d".into(), 10),
            (true, vec!["a".into(), "c".into()])
        );
    }
}
