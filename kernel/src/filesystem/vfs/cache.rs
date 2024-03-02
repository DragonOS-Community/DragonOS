
use core::hash::{Hash,Hasher,SipHasher};

use alloc::sync::{Weak, Arc};

use super::IndexNode;

const DEFAULT_MAX_SIZE: u64 = 1024;

#[derive(Debug)]
pub struct DCache<T: IndexNode + ?Sized> {
    curr_size: usize,
    arr: [CacheLine<T>; DEFAULT_MAX_SIZE as usize],
}

#[derive(Debug)]
pub struct CacheLine<T: IndexNode + ?Sized> {
    count: i64,
    entry: Option<Weak<T>>,
}

impl<T: IndexNode + ?Sized> DCache<T> {
    const INIT: CacheLine<T> = CacheLine { count: 0, entry: None };
    fn position(key: &str) -> u64 {
        let mut hasher = SipHasher::new();
        key.hash(&mut hasher);
        hasher.finish() % DEFAULT_MAX_SIZE
    }

    pub fn new() -> DCache<T> {
        DCache {
            curr_size: 0,
            arr: [Self::INIT; DEFAULT_MAX_SIZE as usize],
            // queue: BinaryHeap::new()
        }
    }

    pub fn put(&mut self, name: &str, entry: &Arc<T>) -> Option<Arc<T>> {
        let position = Self::position(name);
        let to_ret: Option<Arc<T>> = self
            .arr[position as usize]
            .entry
            .take()?
            .upgrade()
            .or_else(||{self.curr_size+=1;None});
        self.arr[position as usize].entry = Some(Arc::downgrade(&entry));
        self.arr[position as usize].count = 1;
        to_ret
    }

    pub fn get(&self, key: &str) -> Option<Arc<T>> {
        if let Some(entry) = self
            .arr[Self::position(key) as usize].entry {
            return entry.upgrade().and_then(|en|{
                self.arr[Self::position(key) as usize].count += 1;
                Some(en)
            });
        }
        None
    }

    pub fn remove(&mut self, key: &str) {
        self
            .arr[Self::position(key) as usize]
            .entry
            .take();
    }

    pub fn clear(&mut self) {
        // overwrite the array to yeet everything
        self.curr_size = 0;
        self.arr = [Self::INIT; DEFAULT_MAX_SIZE as usize];
    }

}
