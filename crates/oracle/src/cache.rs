//! Values built from the weather data, kept until newer data or age makes
//! them stale. A stale value is still served while one rebuild runs in the
//! background, so readers wait for a query only the first time a value is
//! asked for.

use std::{
    collections::{HashMap, HashSet},
    hash::Hash,
    time::{Duration, Instant},
};

/// What a lookup found.
#[derive(Debug, PartialEq, Eq)]
pub enum Cached<V> {
    /// Built from the current data, recently enough.
    Fresh(V),
    /// Built from older data, or too long ago. `refresh` is true for the
    /// one caller that should rebuild it; others serve it as it is.
    Stale {
        value: V,
        refresh: bool,
    },
    Missing,
}

struct Entry<V> {
    value: V,
    /// The data generation it was built from.
    generation: u64,
    built: Instant,
    /// When it was last read or written, as a counter.
    used: u64,
}

/// Values by key, the least recently used dropped first once full.
/// Eviction scans all entries, which is cheap at these sizes and only
/// happens when a new key arrives.
pub struct Cache<K, V> {
    entries: HashMap<K, Entry<V>>,
    /// Keys a rebuild is running for.
    refreshing: HashSet<K>,
    uses: u64,
    capacity: usize,
    max_age: Duration,
}

impl<K: Clone + Eq + Hash, V: Clone> Cache<K, V> {
    pub fn new(capacity: usize, max_age: Duration) -> Self {
        Self {
            entries: HashMap::new(),
            refreshing: HashSet::new(),
            uses: 0,
            capacity,
            max_age,
        }
    }

    /// The value for `key`, and whether it is fresh for data `generation`.
    pub fn get(&mut self, key: &K, generation: u64) -> Cached<V> {
        self.uses += 1;
        let uses = self.uses;
        let Some(entry) = self.entries.get_mut(key) else {
            return Cached::Missing;
        };
        entry.used = uses;
        if entry.generation == generation && entry.built.elapsed() < self.max_age {
            return Cached::Fresh(entry.value.clone());
        }
        let value = entry.value.clone();
        let refresh = self.refreshing.insert(key.clone());
        Cached::Stale { value, refresh }
    }

    /// Stores a value built from data `generation`, which the builder read
    /// before it started, so data that arrived meanwhile leaves it stale.
    pub fn insert(&mut self, key: K, value: V, generation: u64) {
        self.uses += 1;
        self.refreshing.remove(&key);
        if self.entries.len() >= self.capacity
            && !self.entries.contains_key(&key)
            && let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.used)
                .map(|(key, _)| key.clone())
        {
            self.entries.remove(&oldest);
        }
        self.entries.insert(
            key,
            Entry {
                value,
                generation,
                built: Instant::now(),
                used: self.uses,
            },
        );
    }

    /// A rebuild failed: the stale value stays, and the next reader retries.
    pub fn refresh_failed(&mut self, key: &K) {
        self.refreshing.remove(key);
    }

    /// Keys read or written among the last `recent` uses, newest first.
    pub fn recent_keys(&self, recent: u64) -> Vec<K> {
        let since = self.uses.saturating_sub(recent);
        let mut keys: Vec<_> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.used > since)
            .map(|(key, entry)| (entry.used, key.clone()))
            .collect();
        keys.sort_by(|a, b| b.0.cmp(&a.0));
        keys.into_iter().map(|(_, key)| key).collect()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_data_makes_values_stale_and_one_reader_rebuilds() {
        let mut cache = Cache::new(4, Duration::from_secs(60));
        assert_eq!(cache.get(&"KORD", 0), Cached::Missing);
        cache.insert("KORD", 1, 0);
        assert_eq!(cache.get(&"KORD", 0), Cached::Fresh(1));
        assert_eq!(
            cache.get(&"KORD", 1),
            Cached::Stale {
                value: 1,
                refresh: true
            }
        );
        assert_eq!(
            cache.get(&"KORD", 1),
            Cached::Stale {
                value: 1,
                refresh: false
            }
        );
        cache.insert("KORD", 2, 1);
        assert_eq!(cache.get(&"KORD", 1), Cached::Fresh(2));
    }

    #[test]
    fn a_failed_rebuild_lets_the_next_reader_retry() {
        let mut cache = Cache::new(4, Duration::from_secs(60));
        cache.insert("KORD", 1, 0);
        assert!(matches!(
            cache.get(&"KORD", 1),
            Cached::Stale { refresh: true, .. }
        ));
        cache.refresh_failed(&"KORD");
        assert!(matches!(
            cache.get(&"KORD", 1),
            Cached::Stale { refresh: true, .. }
        ));
    }

    #[test]
    fn old_values_are_stale() {
        let mut cache = Cache::new(4, Duration::ZERO);
        cache.insert("KORD", 1, 0);
        assert!(matches!(cache.get(&"KORD", 0), Cached::Stale { .. }));
    }

    #[test]
    fn the_least_recently_used_goes_first() {
        let mut cache = Cache::new(2, Duration::from_secs(60));
        cache.insert("a", 1, 0);
        cache.insert("b", 2, 0);
        cache.get(&"a", 0);
        cache.insert("c", 3, 0);
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.recent_keys(2), vec!["c", "a"]);
        assert_eq!(cache.get(&"b", 0), Cached::Missing);
        assert_eq!(cache.get(&"a", 0), Cached::Fresh(1));
    }
}
