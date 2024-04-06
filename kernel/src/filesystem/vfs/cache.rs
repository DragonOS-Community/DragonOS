//! 文件系统目录项缓存
//! Todo: 更改使用的哈希
use alloc::{
    collections::{LinkedList, VecDeque},
    sync::{Arc, Weak},
    vec::Vec,
};
#[allow(deprecated)]
use core::{
    hash::{Hash, Hasher, SipHasher},
    marker::PhantomData,
    mem::{size_of, swap},
    sync::atomic::{AtomicUsize, Ordering},
};

use crate::libs::{
    rwlock::{RwLock, RwLockUpgradableGuard},
    spinlock::SpinLock,
};

use super::IndexNode;
type Resource = Arc<dyn IndexNode>;
// type SrcPtr = Weak<Resource>;
// type SrcManage = Arc<Resource>;

// not thread safe
pub struct SrcIter<'a> {
    idx: usize,
    vec: Option<RwLockUpgradableGuard<'a, VecDeque<Weak<dyn IndexNode>>>>,
}

impl<'a> Iterator for SrcIter<'a> {
    type Item = Arc<dyn IndexNode>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut vec_here = None;
        swap(&mut vec_here, &mut self.vec);
        // let vec_here = core::mem::take(&mut self.vec);
        let mut vec_cur = vec_here.unwrap();

        // 自动删除空节点
        let result = loop {
            if vec_cur.len() <= self.idx {
                break None;
            }
            if let Some(ptr) = vec_cur[self.idx].upgrade() {
                break Some(ptr);
            }
            let mut writer = vec_cur.upgrade();
            writer.remove(self.idx);
            vec_cur = writer.downgrade_to_upgradeable();
        };

        self.vec.replace(vec_cur);
        self.idx += 1;

        result
    }
}

#[derive(Debug)]
struct HashTable<H: Hasher + Default> {
    _hash_type: PhantomData<H>,
    table: Vec<RwLock<VecDeque<Weak<dyn IndexNode>>>>,
}

impl<H: Hasher + Default> HashTable<H> {
    fn new(size: usize) -> Self {
        let mut new = Self {
            _hash_type: PhantomData,
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
    fn put(&self, key: &str, src: Weak<dyn IndexNode>) {
        let mut guard = self.table[self._position(key)].write();
        guard.push_back(src);
    }
}

#[derive(Debug)]
struct LruList {
    list: LinkedList<Resource>,
}

impl LruList {
    fn new() -> Self {
        Self {
            list: LinkedList::new(),
        }
    }

    fn push(&mut self, src: Resource) {
        self.list.push_back(src);
        // kdebug!("List: {:?}", self.list.iter().map(|item| item.clone().upgrade()).collect::<Vec<_>>());
        // Arc::downgrade(&to_put)
    }

    fn clean(&mut self) -> usize {
        kdebug!("Called clean.");
        if self.list.is_empty() {
            return 0;
        }
        self.list
            .extract_if(|src| {
                // 原始指针已被销毁
                if Arc::strong_count(src) < 2 {
                    return true;
                }
                false
            })
            .count()
    }
}

/// Directory Cache 的默认实现
/// Todo: 使用自定义优化哈希函数
#[allow(deprecated)]
pub struct DefaultDCache<H: Hasher + Default = SipHasher> {
    /// hash index
    table: HashTable<H>,
    /// lru note
    deque: SpinLock<LruList>,
    // /// resource release
    // source: SpinLock<CacheManager>,
    max_size: AtomicUsize,
    size: AtomicUsize,
}

impl<H: Hasher + Default> DefaultDCache<H> {
    const DEFAULT_MEMORY_SIZE: usize = 1024 /* K */ * 1024 /* Byte */;
    pub fn new(mem_size: Option<usize>) -> Self {
        let mem_size = mem_size.unwrap_or(Self::DEFAULT_MEMORY_SIZE);
        let max_size =
            mem_size / (2 * size_of::<Arc<dyn IndexNode>>() + size_of::<Weak<dyn IndexNode>>());
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
                // kdebug!("Cache with key {}.", key);
                self.table.put(key, Arc::downgrade(&src));
                self.deque.lock().push(src);
                self.size.fetch_add(1, Ordering::Acquire);
                if self.size.load(Ordering::Acquire) >= self.max_size.load(Ordering::Acquire) {
                    kdebug!("Automately clean.");
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
}

impl<H: Hasher + Default> core::fmt::Debug for DefaultDCache<H> {
    /// 避免在调试时打印过多信息
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "DefaultCache")
    }
}
