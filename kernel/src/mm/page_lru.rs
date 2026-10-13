//! Page-reclaimer membership and recency without per-insertion heap nodes.
//!
//! Links are physical keys in the same map that owns the page reference.
//! Capacity is prepared outside the reclaimer mutex; insertion and promotion
//! then perform no allocations. Retired maps/pages must be dropped unlocked.

use alloc::sync::Arc;
use hashbrown::HashMap;
use system_error::SystemError;

use super::{page::Page, PhysAddr};

struct Entry {
    page: Arc<Page>,
    prev: Option<PhysAddr>,
    next: Option<PhysAddr>,
}

pub(crate) struct PreparedPageLruCapacity {
    entries: HashMap<PhysAddr, Entry>,
}

impl PreparedPageLruCapacity {
    pub(crate) fn new(len: usize, additional: usize) -> Result<Self, SystemError> {
        let required = len.checked_add(additional).ok_or(SystemError::ENOMEM)?;
        let target = len.checked_mul(2).ok_or(SystemError::ENOMEM)?.max(required);
        let mut entries = HashMap::new();
        entries
            .try_reserve(target)
            .map_err(|_| SystemError::ENOMEM)?;
        Ok(Self { entries })
    }

    pub(crate) fn capacity(&self) -> usize {
        self.entries.capacity()
    }
}

pub(crate) struct PageLru {
    entries: HashMap<PhysAddr, Entry>,
    head: Option<PhysAddr>,
    tail: Option<PhysAddr>,
}

impl PageLru {
    pub(crate) fn new() -> Self {
        Self {
            entries: HashMap::new(),
            head: None,
            tail: None,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
    pub(crate) fn capacity(&self) -> usize {
        self.entries.capacity()
    }

    /// Caller already validated capacity against the current live membership.
    /// The returned old table is empty, but its backing storage still needs an
    /// unlocked drop. Moving entries preserves all links and Arc identities.
    pub(crate) fn install_capacity(
        &mut self,
        mut prepared: PreparedPageLruCapacity,
    ) -> PreparedPageLruCapacity {
        assert!(prepared.capacity() >= self.len());
        for (key, entry) in self.entries.drain() {
            assert!(prepared.entries.insert(key, entry).is_none());
        }
        core::mem::swap(&mut self.entries, &mut prepared.entries);
        prepared
    }

    pub(crate) fn peek(&self, key: &PhysAddr) -> Option<&Arc<Page>> {
        self.entries.get(key).map(|entry| &entry.page)
    }

    pub(crate) fn get(&mut self, key: &PhysAddr) -> Option<&Arc<Page>> {
        if !self.entries.contains_key(key) {
            return None;
        }
        self.promote(key);
        self.peek(key)
    }

    pub(crate) fn peek_lru(&self) -> Option<(&PhysAddr, &Arc<Page>)> {
        let key = self.tail.as_ref()?;
        Some((key, self.peek(key).expect("page LRU tail missing")))
    }

    /// Same-key/different-page is an ownership conflict, never replacement.
    /// No capacity growth or last-reference destruction occurs here.
    pub(crate) fn insert_prepared(
        &mut self,
        key: PhysAddr,
        page: &Arc<Page>,
    ) -> Result<(), SystemError> {
        if let Some(current) = self.peek(&key) {
            if !Arc::ptr_eq(current, page) {
                return Err(SystemError::EEXIST);
            }
            self.promote(&key);
            return Ok(());
        }
        if self.len() == self.capacity() {
            return Err(SystemError::ENOMEM);
        }
        let old_head = self.head;
        self.entries.insert(
            key,
            Entry {
                page: page.clone(),
                prev: None,
                next: old_head,
            },
        );
        if let Some(head) = old_head {
            self.entries
                .get_mut(&head)
                .expect("page LRU head missing")
                .prev = Some(key);
        } else {
            self.tail = Some(key);
        }
        self.head = Some(key);
        Ok(())
    }

    fn detach_links(&mut self, key: PhysAddr) {
        let entry = self.entries.get(&key).expect("page LRU unlink missing");
        let (prev, next) = (entry.prev, entry.next);
        if let Some(prev) = prev {
            self.entries
                .get_mut(&prev)
                .expect("page LRU prev missing")
                .next = next;
        } else {
            self.head = next;
        }
        if let Some(next) = next {
            self.entries
                .get_mut(&next)
                .expect("page LRU next missing")
                .prev = prev;
        } else {
            self.tail = prev;
        }
    }

    pub(crate) fn promote(&mut self, key: &PhysAddr) {
        if self.head == Some(*key) || !self.entries.contains_key(key) {
            return;
        }
        self.detach_links(*key);
        let old_head = self.head;
        let entry = self.entries.get_mut(key).expect("page LRU promote missing");
        entry.prev = None;
        entry.next = old_head;
        if let Some(head) = old_head {
            self.entries
                .get_mut(&head)
                .expect("page LRU head missing")
                .prev = Some(*key);
        } else {
            self.tail = Some(*key);
        }
        self.head = Some(*key);
    }

    pub(crate) fn pop(&mut self, key: &PhysAddr) -> Option<Arc<Page>> {
        if !self.entries.contains_key(key) {
            return None;
        }
        self.detach_links(*key);
        self.entries.remove(key).map(|entry| entry.page)
    }

    pub(crate) fn iter(&self) -> Iter<'_> {
        Iter {
            lru: self,
            next: self.head,
            remaining: self.len(),
        }
    }
}

pub(crate) struct Iter<'a> {
    lru: &'a PageLru,
    next: Option<PhysAddr>,
    remaining: usize,
}

impl<'a> Iterator for Iter<'a> {
    type Item = (&'a PhysAddr, &'a Arc<Page>);
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let key = self.next?;
        let (key, entry) = self
            .lru
            .entries
            .get_key_value(&key)
            .expect("page LRU iterator missing");
        self.next = entry.next;
        self.remaining -= 1;
        Some((key, &entry.page))
    }
}
