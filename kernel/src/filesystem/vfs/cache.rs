
use core::hash::{Hash,Hasher,SipHasher};

use alloc::{collections::{linked_list::Cursor, LinkedList}, sync::{Arc, Weak}};

use super::IndexNode;

const DEFAULT_MAX_SIZE: u64 = 1024;

#[derive(Debug)]
pub struct DCache<'a, T: IndexNode + ?Sized> {
    curr_size: usize,
    arr: Vec<Cursor<'a, CacheLine<T>>>,
    lru: LinkedList<CacheLine<T>>,
}

#[derive(Debug)]
pub struct CacheLine<T: IndexNode + ?Sized> {
    count: i64,
    entry: Option<Weak<T>>,
}

impl<'a, T: IndexNode + ?Sized> DCache<'a, T> {
    // const INIT: Cursor<'a, CacheLine<T>> = CacheLine { count: 0, entry: None };
    fn position(key: &str) -> u64 {
        let mut hasher = SipHasher::new();
        key.hash(&mut hasher);
        hasher.finish() % DEFAULT_MAX_SIZE
    }

    pub fn new(max_size: Option<usize>) -> DCache<'a, T> {
        let mut ret = DCache {
            curr_size: 0,
            arr: Vec::new(),
            lru: LinkedList::new(),
        };
        ret.arr.resize(
            max_size.unwrap_or(DEFAULT_MAX_SIZE as usize), 
            ret.lru.cursor_back()
        );
        
        ret
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

    pub fn get(&mut self, key: &str) -> Option<Arc<T>> {
        if let Some(entry) = self
            .arr[Self::position(key) as usize].entry.clone() {
            if let Some(ex) = entry.upgrade() {
                self.arr[Self::position(key) as usize].count += 1;
                return Some(ex);
            }
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
