//! 文件系统目录项缓存
//! Todo: 更改使用的哈希
use alloc::{collections::LinkedList, string::{String, ToString}, sync::{Arc, Weak}};

use core::{
    mem::size_of,
};

use path_base::{Path, PathBuf};
use hashbrown::HashSet;

use crate::{filesystem::vfs::{utils::{Key, Keyable}, IndexNode}, libs::rwlock::RwLock};

use super::MountFSInode;

#[derive(Debug)]
pub struct DCache {
    table: RwLock<HashSet<Key<Resource>>>,
    lru_list: RwLock<LinkedList<Arc<MountFSInode>>>,
    max_size: usize,
}

#[allow(dead_code)]
const DEFAULT_MEMORY_SIZE: usize = 1024 /* K */ * 1024 /* Byte */;


// pub trait DCache {
//     /// 创建一个新的目录项缓存
//     fn new(mem_size: Option<usize>) -> Self;
//     /// 缓存目录项
//     fn put(&self, src: &Arc<MountFSInode>);
//     /// 清除失效目录项，返回清除的数量（可能的usize话）
//     fn clean(&self, num: Option<>) -> Option<usize>;
//     /// 在dcache中快速查找目录项
//     /// - `search_path`: 搜索路径
//     /// - `stop_path`: 停止路径
//     /// - 返回值: 找到的`inode`及其`路径` 或 [`None`]
//     fn quick_lookup<'a> (
//         &self,
//         search_path: &'a Path,
//         stop_path: &'a Path,
//     ) -> Option<(Arc<MountFSInode>, &'a Path)>;
// }

impl DCache {
    pub fn new() -> Self {
        DCache {
            table: RwLock::new(HashSet::new()),
            lru_list: RwLock::new(LinkedList::new()),
            max_size: 0,
        }
    }

    pub fn new_with_max_size(size: usize) -> Self {
        DCache {
            table: RwLock::new(HashSet::new()),
            lru_list: RwLock::new(LinkedList::new()),
            max_size: size,
        }
    }

    pub fn put(&self, src: &Arc<MountFSInode>) {
        self.lru_list.write().push_back(src.clone());
        self.table.write().insert(Key::Inner(Resource(Arc::downgrade(src))));
        if self.max_size != 0 && 
            self.table.read().len() >= self.max_size 
        {
            self.clean();
        }
    }

    pub fn clean(&self) -> usize {
        return 
            self.lru_list.write().extract_if(|elem| 
                Arc::strong_count(&elem.inner_inode) <= 1
            )
            .count();
    }


    pub fn quick_lookup<'b>(
        &self,
        search_path: &'b Path,
        stop_path: &'b Path,
    ) -> Option<(Arc<MountFSInode>, &'b Path)> {
        if let Some(k) = self.table.read().get(&Key::Cmp(Arc::new(String::from(search_path.as_os_str())))) {
            if let Key::Inner(src) = k {
                if src.0.strong_count() > 1 {
                    return Some((src.0.upgrade().unwrap(), search_path));
                }
            }
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

// static mut __DCACHE: Option<DCache> = None;

/// Infinite Init 
// pub fn init_dcache() {
//     unsafe {
//         __DCACHE = Some( DCache {
//             table: RwLock::new(HashSet::new()),
//             lru_list: RwLock::new(LinkedList::new()),
//             max_size: 0,
//         })
//     }
// }

// #[inline]
// pub fn dcache_is_uninit() -> bool {
//     unsafe { return __DCACHE.is_none(); }
// }

// #[allow(dead_code)]
// pub fn init_dcache_with_memory_size(mem_size: usize) {
//     let max_size =
//         mem_size / size_of::<Weak<MountFSInode>>();
//     let hash_table_size = max_size / 7 * 10 /* 0.7 */;
//     unsafe {
//         __DCACHE = Some( DCache {
//             table: RwLock::new(HashSet::with_capacity(hash_table_size)),
//             lru_list: RwLock::new(LinkedList::new()),
//             max_size,
//         })
//     }
// }

// fn instance() -> &'static DCache {
//     unsafe {
//         if __DCACHE.is_none() {
//             init_dcache();
//         }
//         return __DCACHE.as_ref().unwrap();
//     }
// }

// pub fn put(src: &Arc<MountFSInode>) {
//     instance().lru_list.write().push_back(src.clone());
//     instance().table.write().insert(Key::Inner(Resource(Arc::downgrade(src))));
//     if instance().max_size != 0 && 
//         instance().table.read().len() >= instance().max_size 
//     {
//         clean();
//     }
// }

// pub fn clean() -> usize {
//     return 
//         instance().lru_list.write().extract_if(|elem| 
//             Arc::strong_count(&elem.inner_inode) <= 1
//         )
//         .count();
// }

// pub fn quick_lookup<'b>(
//     search_path: &'b Path,
//     stop_path: &'b Path,
// ) -> Option<(Arc<MountFSInode>, &'b Path)> {
//     if let Some(k) = instance().table.read().get(&Key::Cmp(Arc::new(String::from(search_path.as_os_str())))) {
//         if let Key::Inner(src) = k {
//             if let Some(inode) = src.0.upgrade() {
//                 return Some((inode, search_path));
//             }
//         }
//     }
//     if let Some(parent) = search_path.parent() {
//         if parent == stop_path {
//             return None;
//         }
//         return quick_lookup(parent, stop_path);
//     }
//     return None;
// }

#[derive(Debug)]
struct Resource(Weak<MountFSInode>);

impl Keyable for Resource {
    fn key(&self) -> Arc<String> {
        if let Some(src) = self.0.upgrade() {
            return Arc::new(src.abs_path().unwrap().into_os_string());
        }
        return Arc::new(String::new());
    }
}
