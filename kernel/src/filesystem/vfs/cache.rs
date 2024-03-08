/**
 * Todo:
 * [ ] - 注入式的路径比较：path是否需要设计
 * [ ] -
 */
use alloc::{collections::{LinkedList, VecDeque}, sync::{Arc, Weak}, vec::Vec};
use system_error::SystemError;
use core::{hash::{Hash, Hasher, SipHasher}, marker::PhantomData, mem::size_of, ops::Index};

use crate::libs::{rwlock::{RwLock, RwLockReadGuard}, spinlock::SpinLock};

use super::IndexNode;
type Resource = Weak<dyn IndexNode>;
type SrcPtr = Weak<Resource>;
type SrcManage = Arc<Resource>;

struct HashTable<H: Hasher + Default> {
    _hash_type: PhantomData<H>,
    table: Vec<RwLock<LinkedList<SrcPtr>>>,
}

impl<H: Hasher + Default> HashTable<H> {
    fn new(size: usize) -> Self {
        Self {
            _hash_type: PhantomData::default(),
            table: Vec::with_capacity(size)
        }
    }
    /// 下标帮助函数
    fn _position(&self, key: &str) -> usize {
        let mut hasher = H::default();
        key.hash(&mut hasher);
        hasher.finish() as usize % self.table.capacity()
    }
    /// 获取哈希桶
    fn get_list(&self, key: &str) -> RwLockReadGuard<LinkedList<SrcPtr>> {
        self.table[self._position(key)].read()
    }
    /// 插入索引
    fn put(&mut self, key: &str, src: SrcPtr) {
        let mut guard = self.table[self._position(key)].write();
        guard.push_back(src);
    }
}

struct LruList {
    list: VecDeque<SrcPtr>,
}

impl LruList {
    fn new() -> Self {
        Self {
            list: VecDeque::new()
        }
    }

    fn push(&mut self, src: SrcPtr) {
        self.list.push_back(src);
    }
    fn pop(&mut self) {
        if self.list.is_empty() {
            return;
        }
        for iter in 0..self.list.len() {
            self.list.swap(0, iter);
            if self.list[0].upgrade().is_none() {
                self.list.pop_front();
            }
        }
    }
}

struct CacheManager {
    source: LinkedList<SrcManage>,
}

impl CacheManager {
    fn new() -> Self {
        Self {
            source: LinkedList::new(),
        }
    }

    fn add(&mut self, src: Resource) -> SrcPtr {
        let ptr = Arc::new(src);
        let wptr = Arc::downgrade(&ptr);
        self.source.push_back(ptr);
        wptr
    }

    fn release(&mut self) -> usize {
        self.source.extract_if(|src| {
            if src.weak_count() < 2 {
                return true;
            }
            false
        }).count()
    }
}

pub struct DefaultCache<H: Hasher + Default = SipHasher> {
    /// hash index
    table: HashTable<H>,
    /// lru note
    deque: SpinLock<LruList>,
    /// resource release
    source: SpinLock<CacheManager>,

    max_size: usize,
}

impl<H: Hasher + Default> DefaultCache<H> {
    const DEFAULT_MEMORY_SIZE: usize = 1024 /* K */ * 1024 /* Byte */;
    pub fn new(mem_size: Option<usize>) -> Self {
        let mem_size = mem_size.unwrap_or(Self::DEFAULT_MEMORY_SIZE);
        let max_size = mem_size / (2 * size_of::<SrcPtr>() + size_of::<SrcManage>());
        let hash_table_size = max_size / 7 * 10 /* 0.7 */;
        Self {
            table: HashTable::new(hash_table_size),
            deque: SpinLock::new(LruList::new()),
            source: SpinLock::new(CacheManager::new()),
            max_size,
        }
    }

    pub fn put(&mut self, key: &str, src: Resource) {
        let src_p = self.source.lock().add(src);
        self.table.put(key, src_p);
        self.deque.lock().push(src_p);
    }

    pub fn get(&mut self, key: &str) -> RwLockReadGuard<LinkedList<SrcPtr>> {
        self.table.get_list(key)
    }

    pub fn release(&mut self) -> usize {
        self.source.lock().release()
    }

}

trait Cachable<'a>: IndexNode {
    fn name() -> Option<&'a str>;
    fn parent() -> Option<&'a str>;
}
