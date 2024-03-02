
use core::hash::{Hash,Hasher,SipHasher};

use alloc::sync::{Weak, Arc};

use super::IndexNode;

const DEFAULT_MAX_SIZE: u64 = 1024;

#[derive(Debug)]
pub struct DCache {
    curr_size: usize,
    arr: [Weak<dyn IndexNode>; DEFAULT_MAX_SIZE as usize],
}

#[derive(Debug)]
pub struct CacheLine {
    count: i64,
    entry: Weak<dyn IndexNode>,
}

impl DCache {
    const INIT: Weak<dyn IndexNode> = Weak::<dyn IndexNode>::new();

    fn position(key: &str) -> u64 {
        let mut hasher = SipHasher::new();
        key.hash(&mut hasher);
        hasher.finish() % DEFAULT_MAX_SIZE
    }

    pub fn new() -> DCache {
        DCache {
            curr_size: 0,
            arr: [Self::INIT; DEFAULT_MAX_SIZE as usize],
            // queue: BinaryHeap::new()
        }
    }

    pub fn put(&mut self, name: &str, entry: Arc<dyn IndexNode>) -> Option<Arc<dyn IndexNode>> {
        let position = Self::position(name);
        self
            .arr[position as usize]
            .upgrade()
            .replace(entry)
            .or_else(||{self.curr_size+=1;None})
    }

    pub fn get(&self, key: &str) -> Option<Arc<dyn IndexNode>> {
        self.arr[Self::position(key) as usize].upgrade()
            .and_then(|unit| {
                // unit.0.lock().count += 1;
                Some(unit)
            })
    }

    pub fn remove(&mut self, key: &str) -> Option<Arc<dyn IndexNode>> {
        self
            .arr[Self::position(key) as usize]
            .upgrade().take()
            // .take().map(|cu| cu.value)
    }

    pub fn clear(&mut self) {
        // overwrite the array to yeet everything
        self.curr_size = 0;
        self.arr = [Self::INIT; DEFAULT_MAX_SIZE as usize];
    }

}
