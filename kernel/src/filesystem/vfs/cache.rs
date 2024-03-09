/**
 * Todo:
 * [ ] - 注入式的路径比较：path是否需要设计
 * [ ] -
 */
use alloc::{collections::{LinkedList, VecDeque}, sync::{Arc, Weak}, vec::Vec};
use core::{hash::{Hash, Hasher, SipHasher}, marker::PhantomData, mem::size_of};

use crate::libs::{rwlock::{RwLock, RwLockUpgradableGuard}, spinlock::SpinLock};

use super::IndexNode;
type Resource = Weak<dyn IndexNode>;
type SrcPtr = Weak<Resource>;
type SrcManage = Arc<Resource>;

// struct SrcList<'a>(RwLockUpgradableGuard<'a, VecDeque<SrcPtr>>);
struct SrcIter<'a> {
    idx: usize,
    // src: Option<Arc<dyn IndexNode>>,
    vec: RwLockUpgradableGuard<'a, VecDeque<SrcPtr>>,
}

impl<'a> Iterator for SrcIter<'a> {
    type Item = Arc<dyn IndexNode>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.idx == self.vec.len() {
            return None;
        }
        // 自动删除空节点
        while self.vec[self.idx].upgrade().is_none() || 
        self.vec[self.idx].upgrade().unwrap().upgrade().is_none() {
            let mut writer = self.vec.upgrade();
            writer.remove(self.idx);
        }
        self.idx += 1;
        self.vec[self.idx - 1].upgrade().unwrap().upgrade()
    }
}

struct HashTable<H: Hasher + Default> {
    _hash_type: PhantomData<H>,
    table: Vec<RwLock<VecDeque<SrcPtr>>>,
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
    /// 获取哈希桶迭代器
    fn get_list_iter(&self, key: &str) -> SrcIter {
        SrcIter{
            idx: 0,
            vec: self.table[self._position(key)].read()
        }
    }
    /// 插入索引
    fn put(&mut self, key: &str, src: SrcPtr) {
        let mut guard = self.table[self._position(key)].write();
        guard.push_back(src);
    }
}

struct LruList {
    list: LinkedList<SrcManage>,
}

impl LruList {
    fn new() -> Self {
        Self {
            list: LinkedList::new()
        }
    }

    fn push(&mut self, src: Resource) -> SrcPtr {
        let to_put = Arc::new(src);
        self.list.push_back(to_put.clone());
        Arc::downgrade(&to_put)
    }

    fn clean(&mut self) -> usize {
        if self.list.is_empty() {
            return 0;
        }
        self.list.extract_if(|src| {
            // 原始指针已被销毁
            if src.upgrade().is_none() {
                return true
            }
            false
        }).count()
    }

    fn release(&mut self) -> usize {
        if self.list.is_empty() {
            return 0;
        }
        self.list.extract_if(|src| {
            // 原始指针已被销毁
            if src.upgrade().is_none() {
                return true
            }
            // 已无外界在使用该文件
            if src.strong_count() < 2 {
                return true
            }
            false
        }).count()
    }
}

/// Directory Cache 的默认实现
/// Todo: 使用自定义优化哈希函数
pub struct DefaultCache<H: Hasher + Default = SipHasher> {
    /// hash index
    table: HashTable<H>,
    /// lru note
    deque: SpinLock<LruList>,
    // /// resource release
    // source: SpinLock<CacheManager>,

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
            // source: SpinLock::new(CacheManager::new()),
            max_size,
        }
    }

    /// 缓存目录项
    pub fn put(&mut self, key: &str, src: Resource) {
        self.table.put(key, self.deque.lock().push(src));
    }

    /// 获取哈希桶迭代器
    pub fn get(&mut self, key: &str) -> SrcIter {
        self.table.get_list_iter(key)
    }

    /// 清除已被删除的目录项
    pub fn clean(&mut self) -> usize {
        self.deque.lock().clean()
    }

    /// 释放未在使用的目录项与清除已删除的目录项
    pub fn release(&mut self) -> usize {
        self.deque.lock().release()
    }

}

trait Cachable<'a>: IndexNode {
    fn name() -> Option<&'a str>;
    fn parent() -> Option<&'a str>;
}
