//! Fixed-capacity, allocation-free-on-hit memoization cache.
//!
//! [`MemoCache`] is intended for small, per-thread caches of internal shell
//! inputs. It deliberately has no internal locking: callers place it in a
//! `thread_local!` [`std::cell::RefCell`], avoiding global mutex contention at
//! the cost of one cache allocation per thread.

use std::{
	borrow::Borrow,
	hash::{Hash, BuildHasher as _},
	mem,
};

use crate::fasthash::FastState;

/// A fixed-capacity hash cache with second-chance eviction.
///
/// The open-addressed table and all slots are allocated by [`Self::new`] and
/// never resized. Hits allocate nothing. Inserts allocate nothing beyond any
/// allocation performed by the supplied key and value. CLOCK eviction gives
/// recently accessed entries a second chance while requiring no linked list
/// or per-entry allocation.
///
/// This cache uses a fast, locally seeded non-cryptographic hash. Its intended
/// keys are internal shell source strings rather than attacker-controlled,
/// long-lived map state; the fixed capacity bounds collision and flooding
/// damage. Do not use it as an unbounded security boundary.
pub struct MemoCache<K, V> {
	slots:    Box<[Slot<K, V>]>,
	len:      usize,
	capacity: usize,
	hand:     usize,
	seed:     u64,
}

enum Slot<K, V> {
	Empty,
	Tombstone,
	Occupied { hash: u64, key: K, value: V, referenced: bool },
}

impl<K: Hash + Eq, V> MemoCache<K, V> {
	/// Creates an empty cache holding at most `capacity` entries.
	///
	/// A zero-capacity cache is valid and never stores entries.
	pub fn new(capacity: usize) -> Self {
		let table_len = capacity.saturating_mul(2).max(1).next_power_of_two();
		let mut slots = Vec::with_capacity(table_len);
		slots.resize_with(table_len, || Slot::Empty);
		let address = slots.as_ptr() as usize as u64;
		Self {
			slots: slots.into_boxed_slice(),
			len: 0,
			capacity,
			hand: 0,
			seed: address ^ 0xa076_1d64_78bd_642f,
		}
	}

	/// Returns a reference to the cached value for `key`, marking it recently
	/// used.
	#[inline]
	pub fn get<Q>(&mut self, key: &Q) -> Option<&V>
	where
		Q: Hash + Eq + ?Sized,
		K: Borrow<Q>,
	{
		let hash = self.hash(key);
		let index = self.find(key, hash)?;
		let Slot::Occupied { value, referenced, .. } = &mut self.slots[index] else {
			unreachable!("find only returns occupied slots")
		};
		*referenced = true;
		Some(value)
	}

	/// Returns a clone of the cached value for `key`.
	#[inline]
	pub fn get_cloned<Q>(&mut self, key: &Q) -> Option<V>
	where
		Q: Hash + Eq + ?Sized,
		K: Borrow<Q>,
		V: Clone,
	{
		self.get(key).cloned()
	}

	/// Inserts `value` for `key`, returning the replaced value when present.
	///
	/// When full, CLOCK gives referenced entries a second chance and evicts the
	/// first cold entry encountered by the hand.
	pub fn insert(&mut self, key: K, value: V) -> Option<V> {
		if self.capacity == 0 {
			return None;
		}

		let hash = self.hash(&key);
		if let Some(index) = self.find(&key, hash) {
			let Slot::Occupied { value: old, referenced, .. } = &mut self.slots[index] else {
				unreachable!("find only returns occupied slots")
			};
			*referenced = true;
			return Some(mem::replace(old, value));
		}

		if self.len == self.capacity {
			self.evict_one();
		}
		let index = self.insertion_slot(hash);
		self.slots[index] = Slot::Occupied { hash, key, value, referenced: false };
		self.len += 1;
		None
	}

	/// Returns the number of cached entries.
	#[inline]
	pub const fn len(&self) -> usize {
		self.len
	}

	/// Returns whether the cache contains no entries.
	#[inline]
	pub const fn is_empty(&self) -> bool {
		self.len == 0
	}

	/// Removes every entry while retaining the allocated slot storage.
	pub fn clear(&mut self) {
		for slot in &mut self.slots {
			*slot = Slot::Empty;
		}
		self.len = 0;
		self.hand = 0;
	}

	#[inline]
	fn hash<Q: Hash + ?Sized>(&self, key: &Q) -> u64 {
		FastState::with_seed(self.seed).hash_one(key)
	}

	fn find<Q>(&self, key: &Q, hash: u64) -> Option<usize>
	where
		Q: Eq + ?Sized,
		K: Borrow<Q>,
	{
		let mask = self.slots.len() - 1;
		let mut index = hash as usize & mask;
		for _ in 0..self.slots.len() {
			match &self.slots[index] {
				Slot::Empty => return None,
				Slot::Occupied { hash: slot_hash, key: slot_key, .. }
					if *slot_hash == hash && slot_key.borrow() == key =>
				{
					return Some(index);
				},
				Slot::Tombstone | Slot::Occupied { .. } => {},
			}
			index = (index + 1) & mask;
		}
		None
	}

	fn insertion_slot(&self, hash: u64) -> usize {
		let mask = self.slots.len() - 1;
		let mut index = hash as usize & mask;
		let mut tombstone = None;
		for _ in 0..self.slots.len() {
			match self.slots[index] {
				Slot::Empty => return tombstone.unwrap_or(index),
				Slot::Tombstone => {
					if tombstone.is_none() {
						tombstone = Some(index);
					}
				},
				Slot::Occupied { .. } => {},
			}
			index = (index + 1) & mask;
		}
		tombstone.expect("eviction leaves an insertion slot")
	}

	fn evict_one(&mut self) {
		loop {
			match &mut self.slots[self.hand] {
				Slot::Occupied { referenced: true, .. } => {
					if let Slot::Occupied { referenced, .. } = &mut self.slots[self.hand] {
						*referenced = false;
					}
				},
				Slot::Occupied { referenced: false, .. } => {
					self.slots[self.hand] = Slot::Tombstone;
					self.len -= 1;
					self.advance_hand();
					return;
				},
				Slot::Empty | Slot::Tombstone => {},
			}
			self.advance_hand();
		}
	}

	#[inline]
	fn advance_hand(&mut self) {
		self.hand = (self.hand + 1) & (self.slots.len() - 1);
	}
}

#[cfg(test)]
mod tests {
	use super::MemoCache;

	#[test]
	fn capacity_is_never_exceeded() {
		let mut cache = MemoCache::new(3);
		for key in 0..100 {
			cache.insert(key, key * 2);
			assert!(cache.len() <= 3);
		}
	}

	#[test]
	fn clock_retains_hot_entry_over_cold_entries() {
		let mut cache = MemoCache::new(3);
		cache.insert("hot", 1);
		cache.insert("cold-a", 2);
		cache.insert("cold-b", 3);
		assert_eq!(cache.get("hot"), Some(&1));
		cache.insert("new", 4);
		assert_eq!(cache.get("hot"), Some(&1));
		assert_eq!(cache.len(), 3);
	}

	#[test]
	fn borrowed_lookup_does_not_require_owned_key() {
		let mut cache = MemoCache::new(2);
		cache.insert(String::from("source"), 7);
		assert_eq!(cache.get("source"), Some(&7));
		assert_eq!(cache.get_cloned("source"), Some(7));
	}

	#[test]
	fn clear_retains_a_usable_empty_cache() {
		let mut cache = MemoCache::new(2);
		cache.insert(1, "one");
		cache.clear();
		assert!(cache.is_empty());
		assert_eq!(cache.get(&1), None);
		cache.insert(2, "two");
		assert_eq!(cache.get(&2), Some(&"two"));
	}

	#[test]
	fn insert_overwrites_without_growing() {
		let mut cache = MemoCache::new(1);
		assert_eq!(cache.insert("key", 1), None);
		assert_eq!(cache.insert("key", 2), Some(1));
		assert_eq!(cache.len(), 1);
		assert_eq!(cache.get("key"), Some(&2));
	}

	#[test]
	fn zero_capacity_never_stores() {
		let mut cache = MemoCache::new(0);
		cache.insert(1, 1);
		assert!(cache.is_empty());
		assert_eq!(cache.get(&1), None);
	}
}
