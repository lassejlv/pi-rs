//! Insertion-ordered scheduler indexes with logarithmic removal. `IndexMap::shift_remove` moves
//! every later entry, so reserving or ending each of many tasks in turn would be quadratic.
use std::{
    collections::{BTreeMap, HashMap},
    hash::Hash,
};

pub(in crate::harness) struct OrderedMap<K, V> {
    order: BTreeMap<u64, K>,
    entries: HashMap<K, (u64, V)>,
    next: u64,
}
impl<K, V> Default for OrderedMap<K, V> {
    fn default() -> Self {
        Self {
            order: BTreeMap::new(),
            entries: HashMap::new(),
            next: 0,
        }
    }
}
impl<K: Copy + Eq + Hash, V> OrderedMap<K, V> {
    pub fn get(&self, key: &K) -> Option<&V> {
        self.entries.get(key).map(|(_, value)| value)
    }
    pub fn contains_key(&self, key: &K) -> bool {
        self.entries.contains_key(key)
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    /// Like `IndexMap::insert`: an existing key keeps its position and takes the new value.
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        if let Some((_, existing)) = self.entries.get_mut(&key) {
            return Some(std::mem::replace(existing, value));
        }
        let position = self.next;
        self.next += 1;
        self.order.insert(position, key);
        self.entries.insert(key, (position, value));
        None
    }
    pub fn shift_remove(&mut self, key: &K) -> Option<V> {
        let (position, value) = self.entries.remove(key)?;
        self.order.remove(&position);
        Some(value)
    }
    pub fn keys(&self) -> impl Iterator<Item = &K> {
        self.order.values()
    }
    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.order.values().map(|key| &self.entries[key].1)
    }
}

#[derive(Default)]
pub(in crate::harness) struct OrderedSet<K>(OrderedMap<K, ()>);
impl<K: Copy + Eq + Hash> OrderedSet<K> {
    /// Like `IndexSet::insert`: true when `key` was absent; a present key keeps its position.
    pub fn insert(&mut self, key: K) -> bool {
        self.0.insert(key, ()).is_none()
    }
    pub fn shift_remove(&mut self, key: &K) -> bool {
        self.0.shift_remove(key).is_some()
    }
    pub fn iter(&self) -> impl Iterator<Item = &K> {
        self.0.keys()
    }
    pub fn contains(&self, key: &K) -> bool {
        self.0.contains_key(key)
    }
    pub fn len(&self) -> usize {
        self.0.entries.len()
    }
}
impl<K: Copy + Eq + Hash> Extend<K> for OrderedSet<K> {
    fn extend<I: IntoIterator<Item = K>>(&mut self, keys: I) {
        for key in keys {
            self.insert(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keeps_insertion_order_across_removal_and_reinsertion() {
        let mut set = OrderedSet::default();
        set.extend([3, 1, 2]);
        assert!(!set.insert(3));
        assert!(set.shift_remove(&1));
        assert!(!set.shift_remove(&1));
        set.insert(1);
        assert_eq!(set.iter().copied().collect::<Vec<_>>(), [3, 2, 1]);
        let mut map = OrderedMap::default();
        map.insert(5, "a");
        map.insert(4, "b");
        assert_eq!(map.insert(5, "c"), Some("a"));
        assert_eq!(map.values().copied().collect::<Vec<_>>(), ["c", "b"]);
        assert_eq!(map.shift_remove(&5), Some("c"));
        assert!(!map.contains_key(&5) && map.get(&4) == Some(&"b") && !map.is_empty());
    }
}
