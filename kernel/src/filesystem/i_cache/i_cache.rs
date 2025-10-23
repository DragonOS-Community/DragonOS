/*
    通用 inode 缓存机制
    
    使用 (文件系统类型, InodeId) 复合键的 inode 缓存系统：
    
    1. 复合键设计
    - 使用 Magic (文件系统类型) + InodeId 作为缓存键
    - 避免不同文件系统间的 InodeId 冲突
    - 支持所有文件系统类型，不仅限于动态文件系统
    
    2. 主要功能
    - cache_inode(): 添加 inode 到缓存
    - lookup_inode(): 根据文件系统类型和 InodeId 查找缓存
    - uncache_inode(): 从缓存中移除 inode
    
    3. 使用场景
    - procfs 等动态文件系统的 inode 复用
    - 减少重复创建相同逻辑文件的开销
    - 提供统一的跨文件系统 inode 缓存机制
    
    注意：缓存的是 inode 结构本身，动态内容仍通过回调函数实时生成

*/

use hashbrown::HashMap;
use alloc::sync::{Arc, Weak};
use crate::filesystem::vfs::{IndexNode, InodeId, Magic, vcore::generate_inode_id};
use crate::filesystem::procfs::data::process_info::ProcessId;
use crate::libs::spinlock::SpinLock;
use crate::process::ProcessControlBlock;
use system_error::SystemError;
use lazy_static::lazy_static;


#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct CacheKey {
    pub fs_magic: Magic,
    pub inode_id: InodeId,
}

impl CacheKey {
    pub fn new(fs_magic: Magic, inode_id: InodeId) -> Self {
        Self { fs_magic, inode_id }
    }
}

pub struct ICache {
    icache: SpinLock<HashMap<CacheKey, Arc<dyn IndexNode>>>,
    // PID到InodeId的映射表，用于procfs等动态文件系统
    // 同时存储PCB的弱引用，用于检测PID复用
    pid_to_inode: SpinLock<HashMap<(Magic, ProcessId), (InodeId, Weak<ProcessControlBlock>)>>,
}

impl ICache {
    pub fn new() -> Self {
        Self {
            icache: SpinLock::new(HashMap::new()),
            pid_to_inode: SpinLock::new(HashMap::new()),
        }
    }

    /// 添加 inode 到缓存
    #[allow(dead_code)]
    pub fn insert(&self, node: Arc<dyn IndexNode>) -> Result<(), SystemError> {
        let metadata = node.metadata()?;
        let fs_magic = node.fs().super_block().magic;
        let key = CacheKey::new(fs_magic, metadata.inode_id);
        self.icache.lock().insert(key, node);
        Ok(())
    }

    /// 从缓存中查找 inode
    #[allow(dead_code)]
    pub fn get(&self, fs_magic: Magic, inode_id: InodeId) -> Option<Arc<dyn IndexNode>> {
        let key = CacheKey::new(fs_magic, inode_id);
        let cache = self.icache.lock();
        cache.get(&key).cloned()
    }

    /// 从缓存中移除 inode
    #[allow(dead_code)]
    pub fn remove(&self, fs_magic: Magic, inode_id: InodeId) -> Option<Arc<dyn IndexNode>> {
        let key = CacheKey::new(fs_magic, inode_id);
        self.icache.lock().remove(&key)
    }

    /// 获取缓存大小
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.icache.lock().len()
    }

    /// 为PID目录分配InodeId（如果不存在则分配新的）
    pub fn allocate_pid_inode_id(&self, fs_magic: Magic, pid: ProcessId, pcb: &Arc<ProcessControlBlock>) -> InodeId {
        let key = (fs_magic, pid);
        let mut pid_map = self.pid_to_inode.lock();
        
        if let Some(&(existing_id, ref weak_pcb)) = pid_map.get(&key) {
            // 检查是否是同一个进程实例（通过比较PCB指针）
            if let Some(cached_pcb) = weak_pcb.upgrade() {
                if Arc::ptr_eq(&cached_pcb, pcb) {
                    // 是同一个进程，返回已有的ID
                    return existing_id;
                }
            }
            // PCB已释放或不是同一个进程，需要重新分配
        }
        
        // 使用全局生成器分配新的InodeId
        let new_id = generate_inode_id();
        pid_map.insert(key, (new_id, Arc::downgrade(pcb)));
        new_id
    }

    /// 根据PID查找对应的InodeId，然后查找缓存的inode
    /// 同时验证PCB是否匹配，避免PID复用问题
    pub fn get_by_pid(&self, fs_magic: Magic, pid: ProcessId, pcb: &Arc<ProcessControlBlock>) -> Option<Arc<dyn IndexNode>> {
        let key = (fs_magic, pid);
        let pid_map = self.pid_to_inode.lock();
        let (inode_id, weak_pcb) = pid_map.get(&key)?;
        
        // 验证是否是同一个进程实例
        let cached_pcb = weak_pcb.upgrade()?;
        if !Arc::ptr_eq(&cached_pcb, pcb) {
            // 不是同一个进程（PID被复用），返回None
            return None;
        }
        
        let cache_key = CacheKey::new(fs_magic, *inode_id);
        drop(pid_map); // 释放锁
        let cache = self.icache.lock();
        cache.get(&cache_key).cloned()
    }

    /// 清理PID相关的缓存（包括映射表和inode缓存）
    pub fn remove_pid_cache(&self, fs_magic: Magic, pid: ProcessId) -> Option<Arc<dyn IndexNode>> {
        let key = (fs_magic, pid);
        let (inode_id, _weak_pcb) = self.pid_to_inode.lock().remove(&key)?;
        let cache_key = CacheKey::new(fs_magic, inode_id);
        self.icache.lock().remove(&cache_key)
    }

    /// 为PID目录缓存inode（自动建立映射关系）
    pub fn insert_pid_inode(&self, fs_magic: Magic, pid: ProcessId, pcb: &Arc<ProcessControlBlock>, node: Arc<dyn IndexNode>) -> Result<(), SystemError> {
        // 分配或获取InodeId
        let inode_id = self.allocate_pid_inode_id(fs_magic, pid, pcb);
        
        // 缓存inode（使用强引用）
        let cache_key = CacheKey::new(fs_magic, inode_id);
        self.icache.lock().insert(cache_key, node);
        Ok(())
    }

    /// 获取缓存统计信息
    pub fn __stats(&self) -> (usize, usize) {
        let cache = self.icache.lock();
        let total = cache.len();
        let pid_mappings = self.pid_to_inode.lock().len();
        (total, pid_mappings)
    }
}

// 全局 ICache 实例
lazy_static! {
    static ref GLOBAL_ICACHE: ICache = ICache::new();
}

pub fn global_icache() -> &'static ICache {
    &GLOBAL_ICACHE
}

pub fn __cache_inode(node: Arc<dyn IndexNode>) -> Result<(), SystemError> {
    global_icache().insert(node)
}

pub fn __lookup_inode(fs_magic: Magic, inode_id: InodeId) -> Option<Arc<dyn IndexNode>> {
    global_icache().get(fs_magic, inode_id)
}

pub fn __uncache_inode(fs_magic: Magic, inode_id: InodeId) -> Option<Arc<dyn IndexNode>> {
    global_icache().remove(fs_magic, inode_id)
}

/// 为PID目录分配InodeId
pub fn allocate_pid_inode_id(fs_magic: Magic, pid: ProcessId, pcb: &Arc<ProcessControlBlock>) -> InodeId {
    global_icache().allocate_pid_inode_id(fs_magic, pid, pcb)
}

/// 根据PID查找缓存的inode（带PCB验证，避免PID复用问题）
pub fn lookup_inode_by_pid(fs_magic: Magic, pid: ProcessId, pcb: &Arc<ProcessControlBlock>) -> Option<Arc<dyn IndexNode>> {
    global_icache().get_by_pid(fs_magic, pid, pcb)
}

/// 缓存PID目录inode
pub fn cache_pid_inode(fs_magic: Magic, pid: ProcessId, pcb: &Arc<ProcessControlBlock>, node: Arc<dyn IndexNode>) -> Result<(), SystemError> {
    global_icache().insert_pid_inode(fs_magic, pid, pcb, node)
}

/// 清理PID相关缓存
pub fn uncache_pid_inode(fs_magic: Magic, pid: ProcessId) -> Option<Arc<dyn IndexNode>> {
    global_icache().remove_pid_cache(fs_magic, pid)
}

