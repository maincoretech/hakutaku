use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Arc;

struct Entry<K> {
    key: K,
    value: Arc<Vec<u8>>,
    referenced: bool,
}

pub(crate) struct ClockCache<K> {
    budget: usize,
    used: usize,
    hand: usize,
    entries: Vec<Option<Entry<K>>>,
    index: HashMap<K, usize>,
}

impl<K: Clone + Eq + Hash> ClockCache<K> {
    pub fn new(budget: usize) -> Self {
        Self {
            budget,
            used: 0,
            hand: 0,
            entries: Vec::new(),
            index: HashMap::new(),
        }
    }

    pub fn get(&mut self, key: &K) -> Option<Arc<Vec<u8>>> {
        let index = *self.index.get(key)?;
        let entry = self.entries.get_mut(index)?.as_mut()?;
        entry.referenced = true;
        Some(Arc::clone(&entry.value))
    }

    pub fn insert(&mut self, key: K, value: Arc<Vec<u8>>) {
        if self.budget == 0 || value.len() > self.budget || self.index.contains_key(&key) {
            return;
        }
        while self.used.saturating_add(value.len()) > self.budget {
            self.evict_one();
        }
        let slot = self
            .entries
            .iter()
            .position(Option::is_none)
            .unwrap_or_else(|| {
                self.entries.push(None);
                self.entries.len() - 1
            });
        self.used += value.len();
        self.index.insert(key.clone(), slot);
        self.entries[slot] = Some(Entry {
            key,
            value,
            referenced: true,
        });
    }

    pub fn clear(&mut self) {
        self.used = 0;
        self.hand = 0;
        self.entries.clear();
        self.index.clear();
    }

    fn evict_one(&mut self) {
        debug_assert!(!self.index.is_empty());
        loop {
            self.hand %= self.entries.len();
            if let Some(entry) = self.entries[self.hand].as_mut() {
                if entry.referenced {
                    entry.referenced = false;
                } else {
                    let entry = self.entries[self.hand].take().expect("occupied clock slot");
                    self.used = self.used.saturating_sub(entry.value.len());
                    self.index.remove(&entry.key);
                    self.hand = (self.hand + 1) % self.entries.len();
                    return;
                }
            }
            self.hand = (self.hand + 1) % self.entries.len();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(len: usize, value: u8) -> Arc<Vec<u8>> {
        Arc::new(vec![value; len])
    }

    #[test]
    fn cache_never_exceeds_its_byte_budget() {
        let mut cache = ClockCache::new(8);
        cache.insert(1, bytes(6, 1));
        cache.insert(2, bytes(6, 2));

        assert!(cache.used <= 8);
        assert_eq!(cache.index.len(), 1);
        assert_eq!(
            cache.get(&2).as_deref().map(Vec::as_slice),
            Some(&[2; 6][..])
        );
    }

    #[test]
    fn clear_releases_every_rebuildable_entry() {
        let mut cache = ClockCache::new(16);
        cache.insert("a", bytes(8, 1));
        cache.insert("b", bytes(8, 2));

        cache.clear();

        assert_eq!(cache.used, 0);
        assert!(cache.entries.is_empty());
        assert!(cache.index.is_empty());
    }

    #[test]
    fn clock_scan_skips_vacant_slots_without_losing_live_entries() {
        let mut cache = ClockCache::new(8);
        cache.insert(1, bytes(4, 1));
        cache.entries.insert(0, None);
        *cache.index.get_mut(&1).unwrap() += 1;
        cache.hand = 0;
        cache.evict_one();
        assert!(cache.index.is_empty());
    }
}
