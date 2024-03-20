use alloc::{
    collections::{LinkedList, VecDeque},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    hash::{Hash, Hasher, SipHasher},
    marker::PhantomData,
    mem::size_of,
    sync::atomic::{AtomicUsize, Ordering},
};

use crate::libs::{
    rwlock::{RwLock, RwLockUpgradableGuard},
    spinlock::SpinLock,
};

use super::IndexNode;
type Resource = Weak<dyn IndexNode>;
type SrcPtr = Weak<Resource>;
type SrcManage = Arc<Resource>;

// not thread safe
pub struct SrcIter<'a> {
    idx: usize,
    vec: Option<RwLockUpgradableGuard<'a, VecDeque<SrcPtr>>>,
}

impl<'a> Iterator for SrcIter<'a> {
    type Item = Arc<dyn IndexNode>;

    fn next(&mut self) -> Option<Self::Item> {
        let vec_here = core::mem::take(&mut self.vec);
        let mut vec_cur = vec_here.unwrap();
        // kdebug!("Hash list RLock!");
        if self.idx == vec_cur.len() {
            return None;
        }
        kdebug!("Something in Hash list! Length: {}", vec_cur.len());
        // 自动删除空节点
        while vec_cur[self.idx].upgrade().is_none()
            || vec_cur[self.idx].upgrade().unwrap().upgrade().is_none()
        {
            let mut writer = vec_cur.upgrade();
            writer.remove(self.idx);
            vec_cur = writer.downgrade_to_upgradeable();
        }
        kdebug!("Finish Empty pop");
        self.idx += 1;
        let result = vec_cur[self.idx - 1].upgrade().unwrap().upgrade();
        self.vec = Some(vec_cur);
        result
    }
}
#[derive(Debug)]
struct HashTable<H: Hasher + Default> {
    _hash_type: PhantomData<H>,
    table: Vec<RwLock<VecDeque<SrcPtr>>>,
}
/* Todo: Change VecDeque to BTreeMap to record depth message. */
impl<H: Hasher + Default> HashTable<H> {
    fn new(size: usize) -> Self {
        let mut new = Self {
            _hash_type: PhantomData::default(),
            table: Vec::with_capacity(size),
        };
        for _ in 0..size {
            new.table.push(RwLock::new(VecDeque::new()));
        }
        new
    }
    /// 下标帮助函数
    fn _position(&self, key: &str) -> usize {
        let mut hasher = H::default();
        key.hash(&mut hasher);
        hasher.finish() as usize % self.table.capacity()
    }
    /// 获取哈希桶迭代器
    fn get_list_iter(&self, key: &str) -> SrcIter {
        SrcIter {
            idx: 0,
            vec: Some(self.table[self._position(key)].upgradeable_read()),
        }
    }
    /// 插入索引
    fn put(&self, key: &str, src: SrcPtr) {
        let mut guard = self.table[self._position(key)].write();
        guard.push_back(src);
    }
}

#[derive(Debug)]
struct LruList {
    list: LinkedList<SrcManage>,
}

impl LruList {
    fn new() -> Self {
        Self {
            list: LinkedList::new(),
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
        self.list
            .extract_if(|src| {
                // 原始指针已被销毁
                if src.upgrade().is_none() {
                    return true;
                }
                false
            })
            .count()
    }

    fn release(&mut self) -> usize {
        if self.list.is_empty() {
            return 0;
        }
        self.list
            .extract_if(|src| {
                // 原始指针已被销毁
                if src.upgrade().is_none() {
                    return true;
                }
                // 已无外界在使用该文件
                if src.strong_count() < 2 {
                    return true;
                }
                false
            })
            .count()
    }
}

/// Directory Cache 的默认实现
/// Todo: 使用自定义优化哈希函数
#[derive(Debug)]
pub struct DefaultCache<H: Hasher + Default = SipHasher> {
    /// hash index
    table: HashTable<H>,
    /// lru note
    deque: SpinLock<LruList>,
    // /// resource release
    // source: SpinLock<CacheManager>,
    max_size: AtomicUsize,
    size: AtomicUsize,
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
            max_size: AtomicUsize::new(max_size),
            size: AtomicUsize::new(0),
        }
    }

    /// 缓存目录项
    pub fn put(&self, key: &str, src: Resource) {
        match key {
            "" => {
                return;
            }
            "." => {
                return;
            }
            ".." => {
                return;
            }
            key => {
                kdebug!("Cache with key {}.", key);
                self.table.put(key, self.deque.lock().push(src));
                self.size.fetch_add(1, Ordering::Acquire);
                if self.size.load(Ordering::Acquire) >= self.max_size.load(Ordering::Acquire) {
                    self.clean();
                }
            }
        }
    }

    /// 获取哈希桶迭代器
    pub fn get(&self, key: &str) -> SrcIter {
        self.table.get_list_iter(key)
    }

    /// 清除已被删除的目录项
    pub fn clean(&self) -> usize {
        let ret = self.deque.lock().clean();
        self.size.fetch_sub(ret, Ordering::Acquire);
        kdebug!("Clean {} empty entry", ret);
        ret
    }

    /// 释放未在使用的目录项与清除已删除的目录项
    pub fn release(&self) -> usize {
        let ret = self.deque.lock().release();
        self.size.fetch_sub(ret, Ordering::Acquire);
        kdebug!("Release {} empty entry", ret);
        ret
    }
}
