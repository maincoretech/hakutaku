use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Arc;

struct Entry<K> {
    key: K,
    value: Arc<[u8]>,
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

    pub fn get(&mut self, key: &K) -> Option<Arc<[u8]>> {
        let index = *self.index.get(key)?;
        let entry = self.entries.get_mut(index)?.as_mut()?;
        entry.referenced = true;
        Some(Arc::clone(&entry.value))
    }

    pub fn insert(&mut self, key: K, value: Arc<[u8]>) {
        if self.budget == 0 || value.len() > self.budget || self.index.contains_key(&key) {
            return;
        }
        while self.used.saturating_add(value.len()) > self.budget {
            if !self.evict_one() {
                return;
            }
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

    fn evict_one(&mut self) -> bool {
        if self.index.is_empty() {
            return false;
        }
        let scan_limit = self.entries.len().saturating_mul(2).max(1);
        for _ in 0..scan_limit {
            if self.entries.is_empty() {
                return false;
            }
            self.hand %= self.entries.len();
            if let Some(entry) = self.entries[self.hand].as_mut() {
                if entry.referenced {
                    entry.referenced = false;
                } else {
                    if let Some(entry) = self.entries[self.hand].take() {
                        self.used = self.used.saturating_sub(entry.value.len());
                        self.index.remove(&entry.key);
                        self.hand = (self.hand + 1) % self.entries.len();
                        return true;
                    }
                }
            }
            self.hand = (self.hand + 1) % self.entries.len();
        }
        false
    }
}
