//! 文件系统目录项缓存
//! Todo: 更改使用的哈希
use alloc::{
    collections::VecDeque,
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
use path_base::Path;

use crate::libs::rwlock::{RwLock, RwLockUpgradableGuard};

use super::IndexNode;
type Resource = Arc<dyn IndexNode>;
// type SrcPtr = Weak<Resource>;
// type SrcManage = Arc<Resource>;

/// # Safety
/// not thread safe
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
    fn position(&self, key: &str) -> usize {
        let mut hasher = H::default();
        key.hash(&mut hasher);
        hasher.finish() as usize % self.table.capacity()
    }
    /// 获取哈希桶迭代器
    fn get_list_iter(&self, key: &str) -> SrcIter {
        SrcIter {
            idx: 0,
            vec: Some(self.table[self.position(key)].upgradeable_read()),
        }
    }
    /// 插入索引
    fn put(&self, key: &str, src: Weak<dyn IndexNode>) {
        let mut guard = self.table[self.position(key)].write();
        guard.push_back(src);
    }

    /// 清除失效项，返回清除数量
    fn clean(&self) -> usize {
        let mut count = 0;
        self.table.iter().for_each(|queue| {
            let mut idx = 0;
            let mut proc = queue.upgradeable_read();
            while idx < proc.len() {
                if proc[idx].strong_count() == 0 {
                    let mut writer = proc.upgrade();
                    writer.remove(idx);
                    proc = writer.downgrade_to_upgradeable();
                    count += 1;
                } else {
                    idx += 1;
                }
            }
        });
        return count;
    }

    /// 清除指定数量的失效项，返回清除数量
    fn clean_with_limit(&self, num: usize) -> usize {
        let mut count = 0;
        'outer: for queue in &self.table {
            let mut idx = 0;
            let mut proc = queue.upgradeable_read();
            while idx < proc.len() {
                if proc[idx].strong_count() == 0 {
                    let mut writer = proc.upgrade();
                    writer.remove(idx);
                    proc = writer.downgrade_to_upgradeable();
                    count += 1;
                    if count >= num {
                        break 'outer;
                    }
                } else {
                    idx += 1;
                }
            }
        }
        return count;
    }
}

/// Directory Cache 的默认实现
/// Todo: 使用自定义优化哈希函数
#[allow(deprecated)]
pub struct DefaultDCache<H: Hasher + Default = SipHasher> {
    /// hash index
    table: HashTable<H>,
    max_size: usize,
    size: AtomicUsize,
}

impl<H: Hasher + Default> DefaultDCache<H> {}

const DEFAULT_MEMORY_SIZE: usize = 1024 /* K */ * 1024 /* Byte */;
pub trait DCache {
    /// 创建一个新的目录项缓存
    fn new(mem_size: Option<usize>) -> Self;
    /// 缓存目录项
    fn put(&self, key: &str, src: Resource);
    /// 清除失效目录项，返回清除的数量（可能的话）
    fn clean(&self, num: Option<usize>) -> Option<usize>;
    /// 在dcache中快速查找目录项
    /// - `search_path`: 搜索路径
    /// - `stop_path`: 停止路径
    /// - 返回值: 找到的`inode`及其`路径` 或 [`None`]
    fn quick_lookup<'a>(
        &self,
        search_path: &'a Path,
        stop_path: &'a Path,
    ) -> Option<(Arc<dyn IndexNode>, &'a Path)>;
}

impl<H: Hasher + Default> DCache for DefaultDCache<H> {
    fn new(mem_size: Option<usize>) -> Self {
        let mem_size = mem_size.unwrap_or(self::DEFAULT_MEMORY_SIZE);
        let max_size =
            mem_size / (2 * size_of::<Arc<dyn IndexNode>>() + size_of::<Weak<dyn IndexNode>>());
        let hash_table_size = max_size / 7 * 10 /* 0.7 */;
        Self {
            table: HashTable::new(hash_table_size),
            max_size,
            size: AtomicUsize::new(0),
        }
    }

    fn put(&self, key: &str, src: Resource) {
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
                self.size.fetch_add(1, Ordering::Acquire);
                if self.size.load(Ordering::Acquire) >= self.max_size {
                    kdebug!("Automately clean.");
                    self.clean(None);
                }
            }
        }
    }

    fn clean(&self, num: Option<usize>) -> Option<usize> {
        if let Some(num) = num {
            return Some(self.table.clean_with_limit(num));
        }
        return Some(self.table.clean());
    }

    fn quick_lookup<'a>(
        &self,
        search_path: &'a Path,
        stop_path: &'a Path,
    ) -> Option<(Arc<dyn IndexNode>, &'a Path)> {
        // kdebug!("Quick lookup: abs {:?}, rest {:?}", abs_path, rest_path);
        let key = search_path.file_name();

        let result = self.table.get_list_iter(key.unwrap()).find(|src| {
            // kdebug!("Src: {:?}, {:?}; Lookup: {:?}, {:?}", src.key(), src.abs_path(), key, abs_path);
            src.abs_path().unwrap() == search_path
        });

        if let Some(inode) = result {
            return Some((inode, search_path));
        }

        if let Some(parent) = search_path.parent() {
            if parent == stop_path {
                return None;
            }
            return self.quick_lookup(parent, stop_path);
        }
        return None;
    }
}

impl<H: Hasher + Default> core::fmt::Debug for DefaultDCache<H> {
    /// 避免在调试时打印过多信息
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "DefaultCache")
    }
}
