use crate::{
    arch::{CurrentTimeArch, MMArch},
    driver::base::device::device_number::{DeviceNumber, Major},
    filesystem::{
        page_cache::{AsyncPageCacheBackend, PageCache},
        vfs::{
            self, syscall::RenameFlags, utils::DName, vcore::generate_inode_id, FilePrivateData,
            IndexNode, InodeFlags, InodeId, InodeMode, InodeRetentionState, SetMetadataMask,
            SpecialNodeData, XattrFlags,
        },
    },
    ipc::pipe::LockedPipeInode,
    libs::{
        casting::DowncastArc,
        mutex::{Mutex, MutexGuard},
        rwsem::{RwSem, RwSemReadGuard},
        spinlock::SpinLock,
        wait_queue::WaitQueue,
    },
    mm::MemoryManagementArch,
    process::{ProcessManager, RawPid},
    sched::sched_yield,
    time::sleep::nanosleep,
    time::{PosixTimeSpec, TimeArch},
};
use alloc::{
    collections::BTreeMap,
    format,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::fmt::Debug;
use kdepends::another_ext4::{self, FileType};
use num::ToPrimitive;
use system_error::SystemError;

use super::filesystem::Ext4FileSystem;

const WHITEOUT_DEV: DeviceNumber = DeviceNumber::new(Major::UNNAMED_MAJOR, 0);

bitflags! {
    /// Inode 脏状态标志位，对应 Linux `inode->i_state` 中的 `I_DIRTY_*` 位。
    pub(super) struct InodeDirtyState: u32 {
        /// 文件大小变更未刷盘，对应 I_DIRTY_SYNC (1 << 0)
        const SIZE_DIRTY    = 1 << 0;
        /// mtime 变更未刷盘，对应 I_DIRTY_DATASYNC (1 << 1)
        const MTIME_DIRTY   = 1 << 1;
        /// atime 变更未刷盘。读路径仅更新缓存，由 inode writeback 持久化。
        const ATIME_DIRTY   = 1 << 2;
        /// 该 inode 已在文件系统 dirty_inodes 队列中。
        const QUEUED        = 1 << 3;
        /// 该 inode 正在执行元数据写回。
        const WRITEBACK     = 1 << 4;
        /// 需要持久化的缓存元数据集合。
        const PERSISTENT_DIRTY = Self::SIZE_DIRTY.bits()
            | Self::MTIME_DIRTY.bits()
            | Self::ATIME_DIRTY.bits();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Ext4InodeLifecycleState {
    Live,
    Freeing,
    Retired,
    Poisoned(SystemError),
}

#[derive(Debug)]
struct Ext4InodeLifecycleInner {
    state: Ext4InodeLifecycleState,
    active_operations: usize,
    operation_owners: BTreeMap<RawPid, usize>,
}

#[derive(Debug)]
pub(super) struct Ext4InodeLifecycle {
    inner: Mutex<Ext4InodeLifecycleInner>,
    link_mutation: Mutex<()>,
    wait_queue: WaitQueue,
}

impl Ext4InodeLifecycle {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Ext4InodeLifecycleInner {
                state: Ext4InodeLifecycleState::Live,
                active_operations: 0,
                operation_owners: BTreeMap::new(),
            }),
            link_mutation: Mutex::new(()),
            wait_queue: WaitQueue::default(),
        })
    }

    pub(super) fn state(&self) -> Ext4InodeLifecycleState {
        self.inner.lock().state.clone()
    }

    /// Serializes link-count mutations for all aliases of this canonical inode.
    pub(super) fn lock_link_mutation(&self) -> MutexGuard<'_, ()> {
        self.link_mutation.lock()
    }

    pub(super) fn begin_operation(self: &Arc<Self>) -> Result<Ext4InodeOperation, SystemError> {
        let owner = ProcessManager::current_pcb().raw_pid();
        let mut inner = self.inner.lock();
        match inner.state.clone() {
            Ext4InodeLifecycleState::Live => {}
            Ext4InodeLifecycleState::Freeing if inner.operation_owners.contains_key(&owner) => {}
            Ext4InodeLifecycleState::Freeing => return Err(SystemError::EBUSY),
            Ext4InodeLifecycleState::Retired => return Err(SystemError::ESTALE),
            Ext4InodeLifecycleState::Poisoned(error) => return Err(error),
        }

        let active_operations = inner
            .active_operations
            .checked_add(1)
            .ok_or(SystemError::EOVERFLOW)?;
        let owner_depth = inner
            .operation_owners
            .get(&owner)
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(SystemError::EOVERFLOW)?;
        inner.active_operations = active_operations;
        inner.operation_owners.insert(owner, owner_depth);
        Ok(Ext4InodeOperation {
            lifecycle: self.clone(),
            owner,
        })
    }

    pub(super) fn begin_freeing(&self) -> Result<(), SystemError> {
        let mut inner = self.inner.lock();
        match inner.state.clone() {
            Ext4InodeLifecycleState::Live => {
                inner.state = Ext4InodeLifecycleState::Freeing;
                Ok(())
            }
            Ext4InodeLifecycleState::Freeing => Err(SystemError::EBUSY),
            Ext4InodeLifecycleState::Retired => Err(SystemError::ESTALE),
            Ext4InodeLifecycleState::Poisoned(error) => Err(error),
        }
    }

    pub(super) fn wait_for_quiescent(&self) {
        self.wait_queue.wait_until(|| {
            let inner = self.inner.lock();
            (inner.active_operations == 0).then_some(())
        });
    }

    pub(super) fn wait_while_freeing(&self) -> Ext4InodeLifecycleState {
        self.wait_queue.wait_until(|| {
            let state = self.inner.lock().state.clone();
            (state != Ext4InodeLifecycleState::Freeing).then_some(state)
        })
    }

    pub(super) fn set_state(&self, state: Ext4InodeLifecycleState) {
        self.inner.lock().state = state;
        self.wait_queue.wake_all();
    }
}

#[must_use]
pub(super) struct Ext4InodeOperation {
    lifecycle: Arc<Ext4InodeLifecycle>,
    owner: RawPid,
}

/// Keeps the ext4 mmap write-preparation critical section alive until the
/// generic page-cache layer has made the page writable and dirty.
pub(super) struct Ext4MmapWriteGuard<'a> {
    _operation: Ext4InodeOperation,
    _size_guard: RwSemReadGuard<'a, ()>,
}

impl Drop for Ext4InodeOperation {
    fn drop(&mut self) {
        let should_wake = {
            let mut inner = self.lifecycle.inner.lock();
            debug_assert!(inner.active_operations > 0);
            inner.active_operations = inner.active_operations.saturating_sub(1);
            let remove_owner =
                if let Some(owner_depth) = inner.operation_owners.get_mut(&self.owner) {
                    debug_assert!(*owner_depth > 0);
                    *owner_depth = owner_depth.saturating_sub(1);
                    *owner_depth == 0
                } else {
                    debug_assert!(false, "missing ext4 lifecycle operation owner");
                    false
                };
            if remove_owner {
                inner.operation_owners.remove(&self.owner);
            }
            inner.active_operations == 0
        };
        if should_wake {
            self.lifecycle.wait_queue.wake_all();
        }
    }
}

type PrivateData<'a> = crate::libs::mutex::MutexGuard<'a, vfs::FilePrivateData>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Ext4InodeTimes {
    pub(super) atime: u32,
    pub(super) mtime: u32,
    pub(super) ctime: u32,
}

impl From<&another_ext4::FileAttr> for Ext4InodeTimes {
    fn from(attr: &another_ext4::FileAttr) -> Self {
        Self {
            atime: attr.atime,
            mtime: attr.mtime,
            ctime: attr.ctime,
        }
    }
}

pub struct Ext4Inode {
    // 对应another_ext4里面的inode号，用于在ext4文件系统中查找相应的inode
    pub(super) inner_inode_num: u32,
    pub(super) fs_ptr: Weak<super::filesystem::Ext4FileSystem>,
    pub(super) page_cache: Option<Arc<PageCache>>,
    pub(super) children: BTreeMap<DName, Arc<LockedExt4Inode>>,
    pub(super) dname: DName,

    // 对应vfs的inode id，用于标识系统中唯一的inode
    pub(super) vfs_inode_id: InodeId,

    // 指向父级IndexNode的Weak指针
    pub(super) parent: Weak<LockedExt4Inode>,

    // 指向自身的Weak指针，用于获取Arc<Self>
    pub(super) self_ref: Weak<LockedExt4Inode>,

    // 特殊节点数据（用于 FIFO 的 pipe inode）
    pub(super) special_node: Option<SpecialNodeData>,

    /// 缓存的文件大小，避免频繁调用 getattr/setattr。
    /// None 表示未初始化（第一次写时从磁盘读取并缓存）。
    pub(super) cached_file_size: Option<u64>,
    /// Linux inode-style authoritative in-memory timestamps. They are loaded
    /// before the canonical inode is published; atime/mtime writeback is lazy.
    pub(super) cached_times: Ext4InodeTimes,
    /// Monotonic sequence for atime cache mutations. Disk commits compare the
    /// sequence, not only the value, so A->B->A updates cannot be lost.
    pub(super) cached_atime_version: u64,
    /// Monotonic sequence for mtime cache mutations; mmap write preparation
    /// updates mtime after releasing io_lock, so setters/writeback use this
    /// sequence to avoid same-second ABA and lost dirty state.
    pub(super) cached_mtime_version: u64,
    /// 脏状态标志位，对应 Linux `inode->i_state & I_DIRTY_*`。
    pub(super) dirty_state: InodeDirtyState,
}

#[derive(Debug)]
pub struct LockedExt4Inode {
    pub(super) inner: Mutex<Ext4Inode>,
    pub(super) io_lock: Mutex<()>,
    pub(super) size_lock: RwSem<()>,
    pub(super) namespace_lock: Mutex<()>,
    pub(super) lifecycle: Arc<Ext4InodeLifecycle>,
    pub(super) retention: InodeRetentionState,
    pub(super) pending_reclaim: SpinLock<Option<another_ext4::InodeReclaimHandle>>,
    pub(super) eviction_scheduled: SpinLock<bool>,
    pub(super) retention_callback_self: Weak<LockedExt4Inode>,
    pub(super) eviction_filesystem: SpinLock<Weak<Ext4FileSystem>>,
}

impl IndexNode for LockedExt4Inode {
    fn append_lock_fs(&self) -> Option<Arc<dyn vfs::FileSystem>> {
        Some(self.fs())
    }

    fn supports_post_write_sync(&self, file_type: vfs::FileType) -> bool {
        file_type == vfs::FileType::File
    }

    fn retention_state(&self) -> Option<&InodeRetentionState> {
        Some(&self.retention)
    }

    fn on_zero_retention(&self) {
        let inode = self.retention_callback_self.upgrade();
        if let Some(inode) = inode {
            let _ = inode.try_schedule_deferred_eviction();
        }
    }

    fn mmap(&self, _start: usize, _len: usize, _offset: usize) -> Result<(), SystemError> {
        Ok(())
    }

    fn open(
        &self,
        _data: crate::libs::mutex::MutexGuard<vfs::FilePrivateData>,
        _mode: &vfs::file::FileFlags,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    fn create(
        &self,
        name: &str,
        file_type: vfs::FileType,
        mode: vfs::InodeMode,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let _operation = self.begin_operation()?;
        let _io = self.io_lock.lock();
        let _namespace = self.namespace_lock.lock();
        let parent_metadata = self.metadata()?;
        let init = vfs::permission::child_inode_init(&parent_metadata, file_type, mode);
        let mut guard = self.inner.lock();
        // another_ext4的高4位是文件类型，低12位是权限
        let file_mode = InodeMode::from(file_type).union(init.mode);
        let file_mode = another_ext4::InodeMode::from_bits_truncate(file_mode.bits() as u16);
        let fs = guard.concret_fs();
        let _reuse = fs.begin_allocation()?;
        let ext4 = &fs.fs;
        // Resolve the parent lifetime before publishing the on-disk name so
        // no fallible parent lookup remains after the namespace transaction.
        let self_arc = guard.self_ref.upgrade().ok_or(SystemError::ENOENT)?;

        let attr = if file_type == vfs::FileType::Dir {
            Self::retry_metadata_contention(|| {
                ext4.mkdir_with_owner_and_attr(
                    guard.inner_inode_num,
                    name,
                    file_mode,
                    another_ext4::InodeOwner {
                        uid: init.uid as u32,
                        gid: init.gid as u32,
                    },
                )
            })?
        } else {
            Self::retry_metadata_contention(|| {
                ext4.create_with_owner_and_attr(
                    guard.inner_inode_num,
                    name,
                    file_mode,
                    another_ext4::InodeOwner {
                        uid: init.uid as u32,
                        gid: init.gid as u32,
                    },
                )
            })?
        };

        let dname = DName::from(name);
        let inode = fs.publish_allocated_inode(
            attr,
            dname.clone(),
            Some(Arc::downgrade(&self_arc)),
            &_reuse,
        )?;
        // 更新 children 缓存
        guard.children.insert(dname, inode.clone());
        drop(guard);
        Ok(inode as Arc<dyn IndexNode>)
    }

    fn create_with_data(
        &self,
        name: &str,
        file_type: vfs::FileType,
        mode: InodeMode,
        data: usize,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        if data == 0 {
            return self.create(name, file_type, mode);
        }

        Err(SystemError::ENOSYS)
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        data: PrivateData,
    ) -> Result<usize, SystemError> {
        let _operation = self.begin_operation()?;
        let len = core::cmp::min(len, buf.len());
        let buf = &mut buf[0..len];

        // 关键修复：不要在持有 Ext4 inode 自旋锁期间调用 PageCache::{read,write}。
        // PageCache 读写路径内部会调用 inode.metadata() 获取文件大小：
        // - prepare_read(): inode.metadata()
        // 若此处持有 inode 锁，则会在 metadata() 再次尝试获取同一把锁而自旋死锁。
        let page_cache = {
            let guard = self.inner.lock();
            guard.page_cache.clone()
        };

        if let Some(page_cache) = page_cache {
            // 性能优化：不再每次 read 都同步更新 atime 到磁盘。
            // 这等同于 Linux 的 noatime 挂载选项，避免每次读取引发
            // read_inode + write_inode 的额外磁盘 I/O。
            page_cache.read(offset, buf)
        } else {
            self.read_direct(offset, len, buf, data)
        }
    }

    fn read_sync(&self, offset: usize, buf: &mut [u8]) -> Result<usize, SystemError> {
        let _operation = self.begin_operation()?;
        let (fs, inode_num) = {
            let guard = self.inner.lock();
            (guard.concret_fs(), guard.inner_inode_num)
        };
        match fs.fs.getattr(inode_num)?.ftype {
            FileType::Directory => Err(SystemError::EISDIR),
            FileType::Unknown => Err(SystemError::EROFS),
            FileType::RegularFile => fs.fs.read(inode_num, offset, buf).map_err(From::from),
            FileType::SymLink => fs.fs.readlink(inode_num, offset, buf).map_err(From::from),
            _ => Err(SystemError::EINVAL),
        }
    }

    fn read_direct(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: crate::libs::mutex::MutexGuard<vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let len = core::cmp::min(len, buf.len());
        self.read_sync(offset, &mut buf[0..len])
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        data: PrivateData,
    ) -> Result<usize, SystemError> {
        let _operation = self.begin_operation()?;
        let len = core::cmp::min(len, buf.len());
        if len == 0 {
            return Ok(0);
        }
        let buf = &buf[0..len];

        let (fs, inode_num, page_cache) = {
            let guard = self.inner.lock();
            (
                guard.concret_fs(),
                guard.inner_inode_num,
                guard.page_cache.clone(),
            )
        };

        if let Some(page_cache) = page_cache {
            let _invalidate = page_cache.invalidate_write();
            let _size_guard = self.size_lock.read();
            let _io_guard = self.io_lock.lock();

            // 使用缓存的文件大小，避免 getattr 磁盘 I/O
            let old_file_size = {
                let cached_size = self.inner.lock().cached_file_size;
                match cached_size {
                    Some(size) => size,
                    None => {
                        let size = fs.fs.getattr(inode_num)?.size;
                        self.inner.lock().cached_file_size = Some(size);
                        size
                    }
                }
            };

            let new_end = offset.checked_add(len).ok_or(SystemError::EFBIG)?;
            let alloc_start = (offset >> MMArch::PAGE_SHIFT) << MMArch::PAGE_SHIFT;
            let alloc_end = new_end
                .checked_add(MMArch::PAGE_SIZE - 1)
                .ok_or(SystemError::EFBIG)?
                & !(MMArch::PAGE_SIZE - 1);
            let alloc_len = alloc_end
                .checked_sub(alloc_start)
                .ok_or(SystemError::EFBIG)?;

            let time = PosixTimeSpec::now().tv_sec.to_u32().unwrap_or_else(|| {
                log::warn!("Failed to get current time, using 0");
                0
            });
            let stats_start = fs
                .fs
                .prepare_stats_enabled()
                .then(CurrentTimeArch::get_cycles);
            let prepare_result = Self::retry_metadata_contention(|| {
                fs.fs.prepare_buffered_write(
                    inode_num,
                    alloc_start,
                    alloc_len,
                    new_end as u64,
                    Some(time),
                )
            });
            if let Some(start) = stats_start {
                fs.fs.record_prepare_elapsed_cycles(
                    CurrentTimeArch::get_cycles().wrapping_sub(start),
                );
            }
            prepare_result?;

            // 写入范围的磁盘块已就绪，现在安全写入 page cache。
            let write_len = PageCache::write(&page_cache, offset, buf)?;
            if write_len > 0 {
                let written_end = offset.checked_add(write_len).ok_or(SystemError::EFBIG)?;
                let current_file_size = core::cmp::max(old_file_size, written_end as u64);
                let self_arc = {
                    let mut guard = self.inner.lock();
                    guard.cached_file_size = Some(current_file_size);
                    guard.cached_times.mtime = time;
                    guard.cached_mtime_version = guard.cached_mtime_version.wrapping_add(1);
                    guard.self_ref.upgrade().ok_or(SystemError::ENOENT)?
                };
                Ext4FileSystem::mark_inode_dirty(
                    &self_arc,
                    InodeDirtyState::SIZE_DIRTY | InodeDirtyState::MTIME_DIRTY,
                )?;
            }

            Ok(write_len)
        } else {
            let _size_guard = self.size_lock.read();
            self.write_direct(offset, len, buf, data)
        }
    }

    fn write_sync(&self, offset: usize, buf: &[u8]) -> Result<usize, SystemError> {
        let _operation = self.begin_operation()?;
        let _io_guard = self.io_lock.lock();
        let (fs, inode_num) = {
            let guard = self.inner.lock();
            (guard.concret_fs(), guard.inner_inode_num)
        };
        match fs.fs.getattr(inode_num)?.ftype {
            FileType::Directory => Err(SystemError::EISDIR),
            FileType::Unknown => Err(SystemError::EROFS),
            // Use write_data_only: blocks are pre-allocated by prepare_buffered_write() in write_at().
            // Using Ext4::write() here would cause it to call write_inode_with_csum()
            // which overwrites the inode's block_count/extent tree with a stale
            // snapshot, causing setattr to re-allocate blocks endlessly until
            // the extent tree overflows (entries > max_entries → EIO).
            FileType::RegularFile => {
                Self::retry_metadata_contention(|| fs.fs.write_data_only(inode_num, offset, buf))
            }
            _ => Err(SystemError::EINVAL),
        }
    }

    fn write_direct(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let len = core::cmp::min(len, buf.len());
        self.write_sync(offset, &buf[0..len])
    }

    fn fs(&self) -> Arc<dyn vfs::FileSystem> {
        self.inner.lock().concret_fs()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn find(&self, name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        let _operation = self.begin_operation()?;
        let _namespace = self.namespace_lock.lock();
        let mut guard = self.inner.lock();
        let dname = DName::from(name);
        if let Some(child) = guard.children.get(&dname) {
            let child = child.clone();
            let fs = guard.concret_fs();
            if fs.validate_inode(&child).is_ok() {
                return Ok(child as Arc<dyn IndexNode>);
            }
            guard.children.remove(&dname);
        }
        let fs = guard.concret_fs();
        let next_inode = fs.fs.lookup(guard.inner_inode_num, name)?;
        // 通过self_ref获取Arc<Self>，然后转换为Arc<dyn IndexNode>
        let self_arc = guard.self_ref.upgrade().ok_or(SystemError::ENOENT)?;
        let inode =
            fs.get_or_create_inode(next_inode, dname.clone(), Some(Arc::downgrade(&self_arc)))?;
        guard.children.insert(dname, inode.clone());
        Ok(inode)
    }

    fn parent(&self) -> Result<Arc<dyn IndexNode>, SystemError> {
        // 只有目录才有父目录的概念
        // 先检查当前inode是否为目录
        let guard = self.inner.lock();

        // 如果存储了父级指针，直接返回
        if let Some(parent) = guard.parent.upgrade() {
            return Ok(parent);
        }

        Err(SystemError::ENOENT)
    }

    fn list(&self) -> Result<Vec<String>, SystemError> {
        let _operation = self.begin_operation()?;
        let guard = self.inner.lock();
        let dentry = guard.concret_fs().fs.listdir(guard.inner_inode_num)?;
        let mut list = Vec::new();
        for entry in dentry {
            list.push(entry.name());
        }
        Ok(list)
    }

    fn link(&self, name: &str, other: &Arc<dyn IndexNode>) -> Result<(), SystemError> {
        let _operation = self.begin_operation()?;
        let _namespace = self.namespace_lock.lock();
        let mut guard = self.inner.lock();
        let fs = guard.concret_fs();
        let ext4 = &fs.fs;
        let inode_num = guard.inner_inode_num;

        let other_arc = other
            .clone()
            .downcast_arc::<LockedExt4Inode>()
            .ok_or(SystemError::EINVAL)?;
        let other_fs = other_arc.inner.lock().concret_fs();
        if !Arc::ptr_eq(&fs, &other_fs) {
            return Err(SystemError::EXDEV);
        }
        let other_lifecycle = other_arc.lifecycle().clone();
        let _link_mutation = other_lifecycle.lock_link_mutation();
        let _other_operation = other_arc.begin_operation()?;
        let other_inode_num = other_arc.inner.lock().inner_inode_num;

        let my_attr = ext4.getattr(inode_num)?;
        let other_attr = ext4.getattr(other_inode_num)?;

        if my_attr.ftype != another_ext4::FileType::Directory {
            return Err(SystemError::ENOTDIR);
        }

        if other_attr.ftype == another_ext4::FileType::Directory {
            return Err(SystemError::EISDIR);
        }

        if ext4.lookup(inode_num, name).is_ok() {
            return Err(SystemError::EEXIST);
        }

        Self::retry_metadata_contention(|| ext4.link(other_inode_num, inode_num, name))?;
        if other_attr.links == 0 {
            // The orphan-del transaction made this inode live again. Discard
            // the one-shot capability published by its previous final unlink
            // before the fd retention that enabled AT_EMPTY_PATH can vanish.
            other_arc.cancel_deferred_reclaim_after_relink();
        }

        let dname = DName::from(name);
        guard.children.insert(dname, other_arc);

        Ok(())
    }

    fn unlink(&self, name: &str) -> Result<(), SystemError> {
        let _operation = self.begin_operation()?;
        let _namespace = self.namespace_lock.lock();
        let mut guard = self.inner.lock();
        let fs = guard.concret_fs();
        let ext4 = &fs.fs;
        let inode_num = guard.inner_inode_num;
        let attr = ext4.getattr(inode_num)?;
        if attr.ftype != another_ext4::FileType::Directory {
            return Err(SystemError::ENOTDIR);
        }
        let target_num = ext4.lookup(inode_num, name)?;
        if ext4.getattr(target_num)?.ftype == FileType::Directory {
            return Err(SystemError::EISDIR);
        }
        let self_arc = guard.self_ref.upgrade().ok_or(SystemError::ENOENT)?;
        let target = fs.get_or_create_inode(
            target_num,
            DName::from(name),
            Some(Arc::downgrade(&self_arc)),
        )?;
        let target_lifecycle = target.lifecycle().clone();
        let _link_mutation = target_lifecycle.lock_link_mutation();
        let _target_operation = target.begin_operation()?;
        match ext4.lookup(inode_num, name) {
            Ok(current) if current == target_num => {}
            Ok(_) => return Err(SystemError::EAGAIN_OR_EWOULDBLOCK),
            Err(error) => return Err(error.into()),
        }
        let reclaim = Self::retry_metadata_contention(|| ext4.unlink(inode_num, name))?;
        target.handoff_namespace_reclaim(reclaim)?;
        // 清理 children 缓存
        let _ = guard.children.remove(&DName::from(name));
        Ok(())
    }

    fn metadata(&self) -> Result<vfs::Metadata, SystemError> {
        let _operation = self.begin_operation()?;
        let (fs, inode_num, vfs_inode_id, cached_size) = {
            let guard = self.inner.lock();
            (
                guard.concret_fs(),
                guard.inner_inode_num,
                guard.vfs_inode_id,
                guard.cached_file_size,
            )
        };
        let attr = fs.fs.getattr(inode_num)?;
        // Disk attributes provide non-cached fields. Read the authoritative
        // in-memory values afterwards so a concurrent atime update cannot be
        // hidden by a stale pre-getattr snapshot.
        let cached_times = self.inner.lock().cached_times;
        let size = cached_size.unwrap_or(attr.size);

        // dev_id: filesystem device number (st_dev)
        let dev_id = fs.raw_dev.data() as usize;

        // raw_dev: device node's rdev (st_rdev), only for char/block devices
        let raw_dev = if matches!(attr.ftype, FileType::CharacterDev | FileType::BlockDev) {
            let (major, minor) = attr.rdev;
            DeviceNumber::new(
                crate::driver::base::device::device_number::Major::new(major),
                minor,
            )
        } else {
            DeviceNumber::default()
        };

        Ok(vfs::Metadata {
            inode_id: vfs_inode_id,
            size: size as i64,
            blk_size: another_ext4::BLOCK_SIZE,
            blocks: attr.blocks as usize,
            atime: PosixTimeSpec::new(cached_times.atime.into(), 0),
            btime: PosixTimeSpec::new(attr.atime.into(), 0),
            mtime: PosixTimeSpec::new(cached_times.mtime.into(), 0),
            ctime: PosixTimeSpec::new(cached_times.ctime.into(), 0),
            file_type: Self::file_type(attr.ftype),
            mode: InodeMode::from_bits_truncate(attr.perm.bits() as u32),
            flags: InodeFlags::empty(),
            nlinks: attr.links as usize,
            uid: attr.uid as usize,
            gid: attr.gid as usize,
            dev_id,
            raw_dev,
        })
    }

    fn close(&self, _: PrivateData) -> Result<(), SystemError> {
        Ok(())
    }

    fn sync(&self) -> Result<(), SystemError> {
        let _operation = self.begin_operation()?;
        if let Some(page_cache) = self.page_cache() {
            page_cache.manager().sync()?;
        }
        self.flush_metadata(false)?;
        let fs = self.inner.lock().concret_fs();
        fs.finish_sync_durability_boundary()
    }

    fn datasync(&self) -> Result<(), SystemError> {
        let _operation = self.begin_operation()?;
        if let Some(page_cache) = self.page_cache() {
            page_cache.manager().sync()?;
        }
        self.flush_metadata(true)?;
        let fs = self.inner.lock().concret_fs();
        fs.finish_sync_durability_boundary()
    }

    fn sync_file(&self, datasync: bool, _data: PrivateData) -> Result<(), SystemError> {
        if datasync {
            self.datasync()
        } else {
            self.sync()
        }
    }

    fn sync_file_range(
        &self,
        start: usize,
        end: usize,
        datasync: bool,
        _data: PrivateData,
    ) -> Result<(), SystemError> {
        let _operation = self.begin_operation()?;
        if let Some(page_cache) = self.page_cache() {
            let start_index = start >> MMArch::PAGE_SHIFT;
            let end_index = end >> MMArch::PAGE_SHIFT;
            page_cache
                .manager()
                .writeback_range(start_index, end_index)?;
        }
        self.flush_metadata(datasync)?;
        let fs = self.inner.lock().concret_fs();
        fs.finish_sync_durability_boundary()
    }

    fn write_inode(&self, _wbc: &vfs::WritebackControl) -> Result<(), SystemError> {
        self.flush_metadata(false)
    }

    fn page_cache(&self) -> Option<Arc<PageCache>> {
        self.inner.lock().page_cache.clone()
    }

    fn set_metadata(&self, metadata: &vfs::Metadata) -> Result<(), SystemError> {
        let _operation = self.begin_operation()?;
        let _io_guard = self.io_lock.lock();
        let mode = metadata.mode.union(InodeMode::from(metadata.file_type));

        let to_ext4_time =
            |time: &PosixTimeSpec| -> u32 { time.tv_sec.max(0).min(u32::MAX as i64) as u32 };

        let (fs, inode_num, before_atime_version, before_mtime_version) = {
            let guard = self.inner.lock();
            (
                guard.concret_fs(),
                guard.inner_inode_num,
                guard.cached_atime_version,
                guard.cached_mtime_version,
            )
        };
        let ext4 = &fs.fs;
        Self::retry_metadata_contention(|| {
            ext4.setattr(
                inode_num,
                another_ext4::SetAttr {
                    mode: Some(another_ext4::InodeMode::from_bits_truncate(
                        mode.bits() as u16
                    )),
                    uid: Some(metadata.uid as u32),
                    gid: Some(metadata.gid as u32),
                    size: Some(metadata.size as u64),
                    atime: Some(to_ext4_time(&metadata.atime)),
                    mtime: Some(to_ext4_time(&metadata.mtime)),
                    ctime: Some(to_ext4_time(&metadata.ctime)),
                    crtime: Some(to_ext4_time(&metadata.btime)),
                },
            )
        })?;
        {
            let mut guard = self.inner.lock();
            guard.cached_file_size = Some(metadata.size as u64);
            if guard.cached_atime_version == before_atime_version {
                guard.cached_times.atime = to_ext4_time(&metadata.atime);
                guard.cached_atime_version = guard.cached_atime_version.wrapping_add(1);
                guard.dirty_state.remove(InodeDirtyState::ATIME_DIRTY);
            }
            if guard.cached_mtime_version == before_mtime_version {
                guard.cached_times.mtime = to_ext4_time(&metadata.mtime);
                guard.cached_mtime_version = guard.cached_mtime_version.wrapping_add(1);
                guard.dirty_state.remove(InodeDirtyState::MTIME_DIRTY);
            }
            guard.cached_times.ctime = to_ext4_time(&metadata.ctime);
            guard.dirty_state.remove(InodeDirtyState::SIZE_DIRTY);
        }
        self.release_clean_metadata_queue_owner(&fs);

        Ok(())
    }

    fn set_metadata_masked(
        &self,
        metadata: &vfs::Metadata,
        mask: SetMetadataMask,
    ) -> Result<(), SystemError> {
        if mask.is_empty() {
            return Ok(());
        }

        let _operation = self.begin_operation()?;
        let _io_guard = self.io_lock.lock();
        let to_ext4_time =
            |time: &PosixTimeSpec| -> u32 { time.tv_sec.max(0).min(u32::MAX as i64) as u32 };
        let (fs, inode_num, before_atime_version, before_mtime_version) = {
            let guard = self.inner.lock();
            (
                guard.concret_fs(),
                guard.inner_inode_num,
                guard.cached_atime_version,
                guard.cached_mtime_version,
            )
        };
        let mode = metadata.mode.union(InodeMode::from(metadata.file_type));
        let atime = mask
            .contains(SetMetadataMask::ATIME)
            .then(|| to_ext4_time(&metadata.atime));
        let mtime = mask
            .contains(SetMetadataMask::MTIME)
            .then(|| to_ext4_time(&metadata.mtime));
        let ctime = mask
            .contains(SetMetadataMask::CTIME)
            .then(|| to_ext4_time(&metadata.ctime));

        Self::retry_metadata_contention(|| {
            fs.fs.setattr(
                inode_num,
                another_ext4::SetAttr {
                    mode: mask
                        .contains(SetMetadataMask::MODE)
                        .then(|| another_ext4::InodeMode::from_bits_truncate(mode.bits() as u16)),
                    uid: mask
                        .contains(SetMetadataMask::UID)
                        .then_some(metadata.uid as u32),
                    gid: mask
                        .contains(SetMetadataMask::GID)
                        .then_some(metadata.gid as u32),
                    atime,
                    mtime,
                    ctime,
                    ..Default::default()
                },
            )
        })?;

        {
            let mut guard = self.inner.lock();
            // Buffered reads/writes can update cached times without io_lock.
            // Preserve and leave dirty any value that changed while setattr
            // was in flight; writeback will then persist the newer value.
            if let Some(atime) = atime {
                if guard.cached_atime_version == before_atime_version {
                    guard.cached_times.atime = atime;
                    guard.cached_atime_version = guard.cached_atime_version.wrapping_add(1);
                    guard.dirty_state.remove(InodeDirtyState::ATIME_DIRTY);
                }
            }
            if let Some(mtime) = mtime {
                if guard.cached_mtime_version == before_mtime_version {
                    guard.cached_times.mtime = mtime;
                    guard.cached_mtime_version = guard.cached_mtime_version.wrapping_add(1);
                    guard.dirty_state.remove(InodeDirtyState::MTIME_DIRTY);
                }
            }
            if let Some(ctime) = ctime {
                guard.cached_times.ctime = ctime;
            }
        }
        self.release_clean_metadata_queue_owner(&fs);
        Ok(())
    }

    fn update_atime(&self, now: PosixTimeSpec, relatime: bool) -> Result<(), SystemError> {
        let atime = now.tv_sec.max(0).min(u32::MAX as i64) as u32;
        let now = PosixTimeSpec::new(atime.into(), 0);
        let self_arc = {
            let guard = self.inner.lock();
            let times = guard.cached_times;
            if !vfs::should_update_atime(
                PosixTimeSpec::new(times.atime.into(), 0),
                PosixTimeSpec::new(times.mtime.into(), 0),
                PosixTimeSpec::new(times.ctime.into(), 0),
                now,
                relatime,
            ) {
                return Ok(());
            }
            guard.self_ref.upgrade().ok_or(SystemError::ENOENT)?
        };
        Ext4FileSystem::mark_inode_atime_dirty(&self_arc, atime, relatime)
    }

    fn resize(&self, len: usize) -> Result<(), SystemError> {
        let _operation = self.begin_operation()?;
        let (fs, inode_num, page_cache) = {
            let guard = self.inner.lock();
            (
                guard.concret_fs(),
                guard.inner_inode_num,
                guard.page_cache.clone(),
            )
        };
        let apply_resize = || -> Result<(), SystemError> {
            let _io_guard = self.io_lock.lock();
            let ext4 = &fs.fs;
            // 仅调整文件大小，其他属性保持不变
            Self::retry_metadata_contention(|| {
                ext4.setattr(
                    inode_num,
                    another_ext4::SetAttr {
                        mode: None,
                        uid: None,
                        gid: None,
                        size: Some(len as u64),
                        atime: None,
                        mtime: None,
                        ctime: None,
                        crtime: None,
                    },
                )
            })?;
            // 更新缓存的文件大小
            {
                let mut guard = self.inner.lock();
                guard.cached_file_size = Some(len as u64);
                guard.dirty_state.remove(InodeDirtyState::SIZE_DIRTY);
            }
            self.release_clean_metadata_queue_owner(&fs);
            Ok(())
        };

        if let Some(page_cache) = page_cache {
            let hole_start_page = len
                .checked_add(MMArch::PAGE_SIZE - 1)
                .ok_or(SystemError::EFBIG)?
                >> MMArch::PAGE_SHIFT;
            let mut truncate_pending = false;
            loop {
                // Match PageCache::truncate(), but acquire ext4's size lock
                // after invalidate_write so mmap faults and regular writes
                // use one global order: invalidate -> size -> inode I/O.
                page_cache.unmap_mapping_pages_even_cow(hole_start_page, None)?;
                let (shrinking, committed) = {
                    let _invalidate = page_cache.invalidate_write();
                    let _size_guard = self.size_lock.write();
                    // Classify against the authoritative size while holding the
                    // same lock that serializes the update.  A function-entry
                    // snapshot can become stale after a concurrent extension.
                    let cached_size = self.inner.lock().cached_file_size;
                    let current_size = match cached_size {
                        Some(size) => size,
                        None => fs.fs.getattr(inode_num)?.size,
                    };
                    // After truncate_locked() asks for another unmap pass, the
                    // inode size already equals len.  Preserve that pending
                    // cache truncation unless a concurrent resize moved the
                    // authoritative size below this request.
                    let shrinking = len < current_size as usize
                        || (truncate_pending && len == current_size as usize);
                    apply_resize()?;
                    let committed = !shrinking || page_cache.truncate_locked(len)?;
                    (shrinking, committed)
                };
                if committed {
                    if shrinking {
                        page_cache.unmap_mapping_pages_even_cow(hole_start_page, None)?;
                    }
                    return Ok(());
                }
                truncate_pending = shrinking;
            }
        }
        let _size_guard = self.size_lock.write();
        apply_resize()
    }

    fn fallocate_file(
        &self,
        mode: i32,
        offset: usize,
        len: usize,
        lock_owner: u64,
        data: MutexGuard<FilePrivateData>,
    ) -> Result<(), SystemError> {
        drop(data);
        vfs::vcore::resize_based_fallocate(self, mode, offset, len, lock_owner)
    }

    fn truncate(&self, len: usize) -> Result<(), SystemError> {
        // 复用 resize 的实现
        self.resize(len)
    }

    fn rmdir(&self, name: &str) -> Result<(), SystemError> {
        let _operation = self.begin_operation()?;
        let _namespace = self.namespace_lock.lock();
        let mut guard = self.inner.lock();
        let fs = guard.concret_fs();
        let concret_fs = &fs.fs;
        let inode_num = guard.inner_inode_num;
        if concret_fs.getattr(inode_num)?.ftype != FileType::Directory {
            return Err(SystemError::ENOTDIR);
        }
        let target_num = concret_fs.lookup(inode_num, name)?;
        if target_num == inode_num {
            return Err(if name == "." {
                SystemError::EINVAL
            } else {
                SystemError::ENOTEMPTY
            });
        }
        if concret_fs.getattr(target_num)?.ftype != FileType::Directory {
            return Err(SystemError::ENOTDIR);
        }
        if concret_fs.listdir(target_num)?.len() > 2 {
            return Err(SystemError::ENOTEMPTY);
        }
        let self_arc = guard.self_ref.upgrade().ok_or(SystemError::ENOENT)?;
        let target = fs.get_or_create_inode(
            target_num,
            DName::from(name),
            Some(Arc::downgrade(&self_arc)),
        )?;
        let target_lifecycle = target.lifecycle().clone();
        let _link_mutation = target_lifecycle.lock_link_mutation();
        match concret_fs.lookup(inode_num, name) {
            Ok(current) if current == target_num => {}
            Ok(_) => return Err(SystemError::EAGAIN_OR_EWOULDBLOCK),
            Err(error) => return Err(error.into()),
        }
        let target_attr = concret_fs.getattr(target_num)?;
        if target_attr.ftype != FileType::Directory {
            return Err(SystemError::ENOTDIR);
        }
        match concret_fs.listdir(target_num) {
            Ok(entries) if entries.len() <= 2 => {}
            Ok(_) => return Err(SystemError::ENOTEMPTY),
            Err(error) => return Err(error.into()),
        }
        let reclaim = Self::retry_metadata_contention(|| concret_fs.rmdir(inode_num, name))?;
        target.handoff_namespace_reclaim(reclaim)?;
        // 清理 children 缓存
        let _ = guard.children.remove(&DName::from(name));

        Ok(())
    }

    fn dname(&self) -> Result<DName, SystemError> {
        Ok(self.inner.lock().dname.clone())
    }

    fn getxattr(&self, name: &str, buf: &mut [u8]) -> Result<usize, SystemError> {
        let _operation = self.begin_operation()?;
        let guard = self.inner.lock();
        let ext4 = &guard.concret_fs().fs;
        let inode_num = guard.inner_inode_num;

        if ext4.getattr(inode_num)?.ftype == FileType::SymLink {
            return Err(SystemError::EPERM);
        }

        // 调用another_ext4库的getxattr接口
        let value = ext4.getxattr(inode_num, name)?;

        // 如果缓冲区为空，只返回需要的长度
        if buf.is_empty() {
            return Ok(value.len());
        }

        // 检查缓冲区大小是否足够
        if buf.len() < value.len() {
            return Err(SystemError::ERANGE);
        }

        // 复制数据到缓冲区
        let copy_len = core::cmp::min(buf.len(), value.len());
        buf[..copy_len].copy_from_slice(&value[..copy_len]);

        Ok(copy_len)
    }

    fn setxattr(&self, name: &str, value: &[u8], flags: XattrFlags) -> Result<usize, SystemError> {
        let _operation = self.begin_operation()?;
        let guard = self.inner.lock();
        let ext4 = &guard.concret_fs().fs;
        let inode_num = guard.inner_inode_num;

        if ext4.getattr(inode_num)?.ftype == FileType::SymLink {
            return Err(SystemError::EPERM);
        }

        Self::retry_metadata_contention(|| {
            ext4.setxattr_with_flags(
                inode_num,
                name,
                value,
                flags.contains(XattrFlags::CREATE),
                flags.contains(XattrFlags::REPLACE),
            )
        })?;

        Ok(0)
    }

    fn listxattr(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        let _operation = self.begin_operation()?;
        let guard = self.inner.lock();
        let ext4 = &guard.concret_fs().fs;
        let inode_num = guard.inner_inode_num;

        let names = ext4.listxattr(inode_num)?;
        let total_len = names.iter().try_fold(0usize, |acc, name| {
            acc.checked_add(name.len())
                .and_then(|len| len.checked_add(1))
                .ok_or(SystemError::E2BIG)
        })?;

        if buf.is_empty() {
            return Ok(total_len);
        }
        if buf.len() < total_len {
            return Err(SystemError::ERANGE);
        }

        let mut offset = 0;
        for name in names {
            let name_bytes = name.as_bytes();
            let next = offset + name_bytes.len();
            buf[offset..next].copy_from_slice(name_bytes);
            buf[next] = 0;
            offset = next + 1;
        }

        Ok(total_len)
    }

    fn removexattr(&self, name: &str) -> Result<usize, SystemError> {
        let _operation = self.begin_operation()?;
        let guard = self.inner.lock();
        let ext4 = &guard.concret_fs().fs;
        let inode_num = guard.inner_inode_num;

        if ext4.getattr(inode_num)?.ftype == FileType::SymLink {
            return Err(SystemError::EPERM);
        }

        Self::retry_metadata_contention(|| ext4.removexattr(inode_num, name))?;
        Ok(0)
    }

    fn mknod(
        &self,
        filename: &str,
        mode: InodeMode,
        dev_t: DeviceNumber,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let file_type = vfs::FileType::from(mode);
        if file_type == vfs::FileType::File {
            return self.create(filename, vfs::FileType::File, mode);
        }
        let _operation = self.begin_operation()?;
        let _io = self.io_lock.lock();
        let _namespace = self.namespace_lock.lock();
        let parent_metadata = self.metadata()?;
        let init = vfs::permission::child_inode_init(&parent_metadata, file_type, mode);

        let mut guard = self.inner.lock();
        let fs = guard.concret_fs();
        let _reuse = fs.begin_allocation()?;
        let ext4 = &fs.fs;
        let inode_num = guard.inner_inode_num;
        // Resolve the parent lifetime before publishing the on-disk name so
        // no fallible parent lookup remains after the namespace transaction.
        let self_arc = guard.self_ref.upgrade().ok_or(SystemError::ENOENT)?;

        if ext4.getattr(inode_num)?.ftype != FileType::Directory {
            return Err(SystemError::ENOTDIR);
        }

        // VFS InodeMode(u32) → another_ext4 InodeMode(u16)
        let file_mode = another_ext4::InodeMode::from_bits_truncate(init.mode.bits() as u16);

        // Create inode based on file type
        let attr = if matches!(
            file_type,
            vfs::FileType::CharDevice | vfs::FileType::BlockDevice
        ) {
            // Character/block device: use mknod to store device number in i_block
            Self::retry_metadata_contention(|| {
                ext4.mknod_with_owner_and_attr(
                    inode_num,
                    filename,
                    file_mode,
                    dev_t.major().data(),
                    dev_t.minor(),
                    another_ext4::InodeOwner {
                        uid: init.uid as u32,
                        gid: init.gid as u32,
                    },
                )
            })?
        } else {
            // FIFO, Socket, etc.: use regular create (no device number needed)
            Self::retry_metadata_contention(|| {
                ext4.create_with_owner_and_attr(
                    inode_num,
                    filename,
                    file_mode,
                    another_ext4::InodeOwner {
                        uid: init.uid as u32,
                        gid: init.gid as u32,
                    },
                )
            })?
        };

        // Wrap as VFS inode and cache
        let dname = DName::from(filename);
        let inode = fs.publish_allocated_inode(
            attr,
            dname.clone(),
            Some(Arc::downgrade(&self_arc)),
            &_reuse,
        )?;
        guard.children.insert(dname, inode.clone());
        drop(guard);
        Ok(inode as Arc<dyn IndexNode>)
    }

    fn special_node(&self) -> Option<SpecialNodeData> {
        self.inner.lock().special_node.clone()
    }

    fn move_to(
        &self,
        old_name: &str,
        target: &Arc<dyn IndexNode>,
        new_name: &str,
        flags: RenameFlags,
    ) -> Result<(), SystemError> {
        let _operation = self.begin_operation()?;
        let _source_io = self.io_lock.lock();
        let whiteout_init = if flags.contains(RenameFlags::WHITEOUT) {
            Some(vfs::permission::child_inode_init(
                &self.metadata()?,
                vfs::FileType::CharDevice,
                InodeMode::S_IFCHR | InodeMode::from_bits_truncate(0o600),
            ))
        } else {
            None
        };
        let target_locked = target
            .clone()
            .downcast_arc::<LockedExt4Inode>()
            .ok_or(SystemError::EXDEV)?;
        let _target_operation = target_locked.begin_operation()?;

        let (ext4_fs, src_inode_num) = {
            let guard = self.inner.lock();
            (guard.concret_fs(), guard.inner_inode_num)
        };
        let ext4 = &ext4_fs.fs;
        let target_inode_num = target_locked.inner.lock().inner_inode_num;
        if !Arc::ptr_eq(&ext4_fs, &target_locked.inner.lock().concret_fs()) {
            return Err(SystemError::EXDEV);
        }

        let (_first_namespace, _second_namespace) = if src_inode_num == target_inode_num {
            (self.namespace_lock.lock(), None)
        } else if src_inode_num < target_inode_num {
            (
                self.namespace_lock.lock(),
                Some(target_locked.namespace_lock.lock()),
            )
        } else {
            (
                target_locked.namespace_lock.lock(),
                Some(self.namespace_lock.lock()),
            )
        };

        let old_dname = DName::from(old_name);
        let new_dname = DName::from(new_name);

        // NOREPLACE check (VFS layer responsibility - ext4 lib doesn't know about flags)
        if flags.contains(RenameFlags::NOREPLACE) && ext4.lookup(target_inode_num, new_name).is_ok()
        {
            return Err(SystemError::EEXIST);
        }

        // Same directory, same name -> no-op
        if src_inode_num == target_inode_num && old_dname == new_dname {
            return Ok(());
        }

        // RENAME_EXCHANGE: 原子交换两个文件/目录
        if flags.contains(RenameFlags::EXCHANGE) {
            // VFS 层已验证目标存在，直接调用 exchange
            Self::retry_metadata_contention(|| {
                ext4.rename_exchange(src_inode_num, old_name, target_inode_num, new_name)
            })?;

            // 更新缓存：交换两个条目
            self.update_exchange_cache(
                &target_locked,
                src_inode_num,
                target_inode_num,
                &old_dname,
                &new_dname,
            );
            return Ok(());
        }

        // Capture the replacement target while both parent namespace locks are held.
        let dst_inode_num = ext4.lookup(target_inode_num, new_name).ok();
        let src_child_num = ext4.lookup(src_inode_num, old_name)?;
        if dst_inode_num == Some(src_child_num) {
            return Ok(());
        }
        let had_dst = dst_inode_num.is_some();
        let dst_inode = if let Some(dst_inode_num) = dst_inode_num {
            let target_parent = target_locked
                .inner
                .lock()
                .self_ref
                .upgrade()
                .ok_or(SystemError::ENOENT)?;
            Some(ext4_fs.get_or_create_inode(
                dst_inode_num,
                new_dname.clone(),
                Some(Arc::downgrade(&target_parent)),
            )?)
        } else {
            None
        };
        let dst_lifecycle = dst_inode.as_ref().map(|inode| inode.lifecycle().clone());
        let _dst_link_mutation = dst_lifecycle
            .as_ref()
            .map(|lifecycle| lifecycle.lock_link_mutation());
        if let Some(dst_inode_num) = dst_inode_num {
            let src_type = ext4.getattr(src_child_num)?.ftype;
            let dst_type = ext4.getattr(dst_inode_num)?.ftype;
            match (
                src_type == FileType::Directory,
                dst_type == FileType::Directory,
            ) {
                (true, false) => return Err(SystemError::ENOTDIR),
                (false, true) => return Err(SystemError::EISDIR),
                (true, true) if ext4.listdir(dst_inode_num)?.len() > 2 => {
                    return Err(SystemError::ENOTEMPTY);
                }
                _ => {}
            }
        }

        let mut resulting_whiteout = None;
        if flags.contains(RenameFlags::WHITEOUT) {
            let whiteout_init = whiteout_init.as_ref().ok_or(SystemError::EIO)?;
            let mut temp_name = String::new();
            let mut whiteout_inode = None;
            let source_parent = self
                .inner
                .lock()
                .self_ref
                .upgrade()
                .ok_or(SystemError::ENOENT)?;
            for _ in 0..32 {
                let candidate = format!(".dragonos-whiteout-{}", generate_inode_id().data());
                if ext4.lookup(src_inode_num, &candidate).is_ok() {
                    continue;
                }
                let allocation = ext4_fs.begin_allocation()?;
                let whiteout_attr = Self::retry_metadata_contention(|| {
                    ext4.mknod_with_owner_and_attr(
                        src_inode_num,
                        &candidate,
                        another_ext4::InodeMode::CHARDEV
                            | another_ext4::InodeMode::from_bits_retain(0o600),
                        WHITEOUT_DEV.major().data(),
                        WHITEOUT_DEV.minor(),
                        another_ext4::InodeOwner {
                            uid: whiteout_init.uid as u32,
                            gid: whiteout_init.gid as u32,
                        },
                    )
                })?;
                whiteout_inode = match ext4_fs.publish_allocated_inode(
                    whiteout_attr,
                    DName::from(candidate.as_str()),
                    Some(Arc::downgrade(&source_parent)),
                    &allocation,
                ) {
                    Ok(inode) => Some(inode),
                    Err(error) => {
                        drop(allocation);
                        let _reclaim = ext4_fs.begin_reclaim();
                        let cleanup = Self::retry_metadata_contention(|| {
                            ext4.unlink(src_inode_num, &candidate)
                        })
                        .and_then(|handle| match handle {
                            Some(handle) => {
                                Self::reclaim_with_metadata_contention_retry(ext4, handle)
                                    .map_err(|failure| SystemError::from(failure.0))
                            }
                            None => Ok(()),
                        });
                        if cleanup.is_err() {
                            ext4_fs.fail_stop_lifecycle();
                            return Err(SystemError::EIO);
                        }
                        return Err(error);
                    }
                };
                temp_name = candidate;
                break;
            }
            if temp_name.is_empty() {
                return Err(SystemError::EEXIST);
            }

            if let Err(err) = Self::retry_metadata_contention(|| {
                ext4.rename_exchange(src_inode_num, old_name, src_inode_num, &temp_name)
            }) {
                Self::reclaim_temporary_inode(
                    &ext4_fs,
                    src_inode_num,
                    &temp_name,
                    whiteout_inode.take().unwrap(),
                )?;
                return Err(err);
            }
            let rename_handle = match Self::retry_metadata_contention(|| {
                ext4.rename(src_inode_num, &temp_name, target_inode_num, new_name)
            }) {
                Ok(handle) => handle,
                Err(rename_error) => {
                    let rollback = Self::retry_metadata_contention(|| {
                        ext4.rename_exchange(src_inode_num, old_name, src_inode_num, &temp_name)
                    });
                    if rollback.is_err() {
                        let whiteout_tombstone = ext4_fs.begin_freeing(
                            whiteout_inode.as_ref().expect("whiteout was published"),
                        )?;
                        let _ = ext4_fs.poison_freeing(whiteout_tombstone, SystemError::EIO);
                        return Err(SystemError::EIO);
                    }
                    Self::reclaim_temporary_inode(
                        &ext4_fs,
                        src_inode_num,
                        &temp_name,
                        whiteout_inode.take().unwrap(),
                    )?;
                    return Err(rename_error);
                }
            };
            if let Some(dst_inode) = &dst_inode {
                dst_inode.handoff_namespace_reclaim(rename_handle)?;
            } else if let Some(handle) = rename_handle {
                // The destination was absent while both namespace locks were
                // held, so the backend must not report a replaced lifetime.
                // If it does, retain that orphan capability and fail-stop.
                return Self::quarantine_unexpected_rename_reclaim(&ext4_fs, handle);
            }
            if let Some(whiteout) = &whiteout_inode {
                whiteout.inner.lock().dname = old_dname.clone();
            }
            resulting_whiteout = whiteout_inode;
        } else {
            if let Some(dst_inode) = &dst_inode {
                let reclaim = Self::retry_metadata_contention(|| {
                    ext4.rename(src_inode_num, old_name, target_inode_num, new_name)
                })?;
                dst_inode.handoff_namespace_reclaim(reclaim)?;
            } else {
                // ext4 library now correctly handles atomic replace
                let reclaim = Self::retry_metadata_contention(|| {
                    ext4.rename(src_inode_num, old_name, target_inode_num, new_name)
                })?;
                if let Some(handle) = reclaim {
                    return Self::quarantine_unexpected_rename_reclaim(&ext4_fs, handle);
                }
            }
        }

        // Update cache
        self.update_rename_cache(
            &target_locked,
            src_inode_num,
            target_inode_num,
            &old_dname,
            &new_dname,
            had_dst,
        );
        if let Some(whiteout) = resulting_whiteout {
            self.inner.lock().children.insert(old_dname, whiteout);
        }
        Ok(())
    }
}

impl LockedExt4Inode {
    fn quarantine_unexpected_rename_reclaim(
        fs: &Arc<Ext4FileSystem>,
        handle: another_ext4::InodeReclaimHandle,
    ) -> Result<(), SystemError> {
        fs.fail_stop_lifecycle();
        // Never risk a second pending capability on a guessed canonical inode.
        // The fail-stopped mount owns this handle until durable orphan recovery
        // can complete after teardown.
        fs.quarantined_reclaims.lock().push(handle);
        Err(SystemError::EIO)
    }

    fn release_clean_metadata_queue_owner(&self, fs: &Arc<Ext4FileSystem>) {
        if let Some(inode) = self.retention_callback_self.upgrade() {
            fs.release_clean_queued_inode(&inode);
        }
    }

    fn metadata_contention_backoff(attempt: usize) {
        const YIELDS_BEFORE_SLEEP: usize = 64;
        if attempt.is_multiple_of(YIELDS_BEFORE_SLEEP) {
            // Keep the current eviction epoch pending, but avoid a workqueue
            // hot loop while an I/O-spanning metadata owner is asleep.
            let _ = nanosleep(PosixTimeSpec::new(0, 1_000_000));
        } else {
            sched_yield();
        }
    }

    pub(super) fn retry_metadata_contention<T>(
        mut operation: impl FnMut() -> core::result::Result<T, another_ext4::Ext4Error>,
    ) -> Result<T, SystemError> {
        let mut attempt = 1usize;
        loop {
            match operation() {
                Ok(value) => return Ok(value),
                Err(error) if error.code() == another_ext4::ErrCode::EAGAIN => {
                    Self::metadata_contention_backoff(attempt);
                    attempt = attempt.saturating_add(1);
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn reclaim_with_metadata_contention_retry(
        fs: &another_ext4::Ext4,
        mut handle: another_ext4::InodeReclaimHandle,
    ) -> Result<(), (another_ext4::Ext4Error, another_ext4::InodeReclaimHandle)> {
        let mut attempt = 1usize;
        loop {
            match fs.reclaim_inode(handle) {
                Ok(()) => return Ok(()),
                Err(failure) => {
                    let (error, returned_handle) = failure.into_parts();
                    if error.code() != another_ext4::ErrCode::EAGAIN {
                        return Err((error, returned_handle));
                    }
                    handle = returned_handle;
                    Self::metadata_contention_backoff(attempt);
                    attempt = attempt.saturating_add(1);
                }
            }
        }
    }

    fn reclaim_temporary_inode(
        fs: &Arc<Ext4FileSystem>,
        parent_inode_num: u32,
        name: &str,
        inode: Arc<LockedExt4Inode>,
    ) -> Result<(), SystemError> {
        let lifecycle = inode.lifecycle().clone();
        let _link_mutation = lifecycle.lock_link_mutation();
        let tombstone = fs.begin_freeing(&inode)?;
        let _reuse = fs.begin_reclaim();
        let mut attempt = 1usize;
        let handle = loop {
            match fs.fs.unlink(parent_inode_num, name) {
                Ok(Some(handle)) => break handle,
                Ok(None) => {
                    let error = SystemError::EIO;
                    let _ = fs.poison_freeing(tombstone, error.clone());
                    return Err(error);
                }
                Err(error) if error.code() == another_ext4::ErrCode::EAGAIN => {
                    Self::metadata_contention_backoff(attempt);
                    attempt = attempt.saturating_add(1);
                }
                Err(error) => {
                    let error = SystemError::from(error);
                    let _ = fs.poison_freeing(tombstone, error.clone());
                    return Err(error);
                }
            }
        };
        if let Err((error, handle)) = Self::reclaim_with_metadata_contention_retry(&fs.fs, handle) {
            *inode.pending_reclaim.lock() = Some(handle);
            let error = SystemError::from(error);
            let _ = fs.poison_freeing(tombstone, error.clone());
            return Err(error);
        }
        fs.complete_freeing(tombstone)
    }

    #[inline]
    fn begin_operation(&self) -> Result<Ext4InodeOperation, SystemError> {
        self.lifecycle.begin_operation()
    }

    pub(super) fn lifecycle(&self) -> &Arc<Ext4InodeLifecycle> {
        &self.lifecycle
    }

    /// 更新 rename 后的缓存
    fn update_rename_cache(
        &self,
        target: &Arc<LockedExt4Inode>,
        src_dir: u32,
        dst_dir: u32,
        old_dname: &DName,
        new_dname: &DName,
        had_dst: bool,
    ) {
        if src_dir == dst_dir {
            let mut guard = self.inner.lock();
            if had_dst {
                guard.children.remove(new_dname);
            }
            if let Some(child) = guard.children.remove(old_dname) {
                child.inner.lock().dname = new_dname.clone();
                guard.children.insert(new_dname.clone(), child);
            }
        } else {
            let (mut src_guard, mut dst_guard) = if src_dir < dst_dir {
                (self.inner.lock(), target.inner.lock())
            } else {
                let d = target.inner.lock();
                let s = self.inner.lock();
                (s, d)
            };

            if had_dst {
                dst_guard.children.remove(new_dname);
            }
            if let Some(child) = src_guard.children.remove(old_dname) {
                dst_guard.children.insert(new_dname.clone(), child.clone());
                drop(src_guard);
                drop(dst_guard);
                let mut child_guard = child.inner.lock();
                child_guard.dname = new_dname.clone();
                child_guard.parent = Arc::downgrade(target);
            }
        }
    }

    /// 更新 exchange 后的缓存：交换两个条目
    fn update_exchange_cache(
        &self,
        target: &Arc<LockedExt4Inode>,
        src_dir: u32,
        dst_dir: u32,
        old_dname: &DName,
        new_dname: &DName,
    ) {
        if src_dir == dst_dir {
            // 同目录交换
            let mut guard = self.inner.lock();
            let old_child = guard.children.remove(old_dname);
            let new_child = guard.children.remove(new_dname);

            if let Some(child) = old_child {
                child.inner.lock().dname = new_dname.clone();
                guard.children.insert(new_dname.clone(), child);
            }
            if let Some(child) = new_child {
                child.inner.lock().dname = old_dname.clone();
                guard.children.insert(old_dname.clone(), child);
            }
        } else {
            // 跨目录交换
            let (mut src_guard, mut dst_guard) = if src_dir < dst_dir {
                (self.inner.lock(), target.inner.lock())
            } else {
                let d = target.inner.lock();
                let s = self.inner.lock();
                (s, d)
            };

            let old_child = src_guard.children.remove(old_dname);
            let new_child = dst_guard.children.remove(new_dname);

            // old_child 移到 target 目录
            if let Some(child) = old_child {
                dst_guard.children.insert(new_dname.clone(), child.clone());
                drop(src_guard);
                drop(dst_guard);

                let mut child_guard = child.inner.lock();
                child_guard.dname = new_dname.clone();
                child_guard.parent = Arc::downgrade(target);
                drop(child_guard);

                // 重新获取锁处理 new_child
                if let Some(new_c) = new_child {
                    let mut src_guard = self.inner.lock();
                    src_guard.children.insert(old_dname.clone(), new_c.clone());
                    drop(src_guard);

                    let mut new_c_guard = new_c.inner.lock();
                    new_c_guard.dname = old_dname.clone();
                    new_c_guard.parent = self.inner.lock().self_ref.clone();
                }
            } else if let Some(new_c) = new_child {
                // 只有 new_child 在缓存中
                src_guard.children.insert(old_dname.clone(), new_c.clone());
                drop(src_guard);
                drop(dst_guard);

                let mut new_c_guard = new_c.inner.lock();
                new_c_guard.dname = old_dname.clone();
                new_c_guard.parent = self.inner.lock().self_ref.clone();
            }
        }
    }

    pub fn new(
        inode_num: u32,
        fs_ptr: Weak<super::filesystem::Ext4FileSystem>,
        dname: DName,
        parent: Option<Weak<LockedExt4Inode>>,
    ) -> Result<Arc<Self>, SystemError> {
        let fs = fs_ptr.upgrade().ok_or(SystemError::EIO)?;
        let attr = fs.fs.getattr(inode_num)?;
        Self::new_with_attr(inode_num, fs_ptr, dname, parent, &attr)
    }

    pub(super) fn new_with_attr(
        inode_num: u32,
        fs_ptr: Weak<super::filesystem::Ext4FileSystem>,
        dname: DName,
        parent: Option<Weak<LockedExt4Inode>>,
        attr: &another_ext4::FileAttr,
    ) -> Result<Arc<Self>, SystemError> {
        debug_assert_eq!(inode_num, attr.ino);
        let lifecycle = Ext4InodeLifecycle::new();
        let inode = Arc::new_cyclic(|self_ref| LockedExt4Inode {
            inner: Mutex::new(Ext4Inode::new(
                inode_num,
                fs_ptr.clone(),
                dname,
                parent,
                Ext4InodeTimes::from(attr),
            )),
            io_lock: Mutex::new(()),
            size_lock: RwSem::new(()),
            namespace_lock: Mutex::new(()),
            lifecycle,
            retention: InodeRetentionState::new(),
            pending_reclaim: SpinLock::new(None),
            eviction_scheduled: SpinLock::new(false),
            retention_callback_self: self_ref.clone(),
            eviction_filesystem: SpinLock::new(fs_ptr.clone()),
        });
        let mut guard = inode.inner.lock();

        // 设置self_ref
        guard.self_ref = Arc::downgrade(&inode);

        let backend = Arc::new(AsyncPageCacheBackend::new(
            Arc::downgrade(&inode) as Weak<dyn IndexNode>
        ));
        let page_cache = PageCache::new(
            Some(Arc::downgrade(&inode) as Weak<dyn IndexNode>),
            Some(backend),
        );
        guard.page_cache = Some(page_cache);

        // 对于 FIFO，创建 pipe inode
        if attr.ftype == FileType::Fifo {
            let pipe_inode = LockedPipeInode::new();
            pipe_inode.set_fifo();
            guard.special_node = Some(SpecialNodeData::Pipe(pipe_inode));
        }

        drop(guard);
        Ok(inode)
    }

    fn file_type(ftype: FileType) -> vfs::FileType {
        match ftype {
            FileType::RegularFile => vfs::FileType::File,
            FileType::Directory => vfs::FileType::Dir,
            FileType::CharacterDev => vfs::FileType::CharDevice,
            FileType::BlockDev => vfs::FileType::BlockDevice,
            FileType::Fifo => vfs::FileType::Pipe,
            FileType::Socket => vfs::FileType::Socket,
            FileType::SymLink => vfs::FileType::SymLink,
            _ => {
                log::warn!("Unknown file type, going to treat it as a file");
                vfs::FileType::File
            }
        }
    }
}

impl Ext4Inode {
    fn concret_fs(&self) -> Arc<Ext4FileSystem> {
        self.fs_ptr
            .upgrade()
            .expect("Ext4FileSystem should be alive")
    }

    pub(super) fn new(
        inode_num: u32,
        fs_ptr: Weak<Ext4FileSystem>,
        dname: DName,
        parent: Option<Weak<LockedExt4Inode>>,
        times: Ext4InodeTimes,
    ) -> Self {
        Self {
            inner_inode_num: inode_num,
            fs_ptr,
            page_cache: None,
            children: BTreeMap::new(),
            dname,
            vfs_inode_id: generate_inode_id(),
            parent: parent.unwrap_or_default(),
            self_ref: Weak::new(), // 将在LockedExt4Inode::new()中设置
            special_node: None,
            cached_file_size: None,
            cached_times: times,
            cached_atime_version: 0,
            cached_mtime_version: 0,
            dirty_state: InodeDirtyState::empty(),
        }
    }
}

impl LockedExt4Inode {
    /// Transfer the authoritative result of a namespace transaction to this
    /// canonical inode lifetime. `None` means another hard link remains;
    /// `Some` is the unique capability for the zero-link orphan.
    fn handoff_namespace_reclaim(
        self: &Arc<Self>,
        reclaim: Option<another_ext4::InodeReclaimHandle>,
    ) -> Result<(), SystemError> {
        let Some(handle) = reclaim else {
            return Ok(());
        };
        let (fs, inode_num) = {
            let inner = self.inner.lock();
            (inner.concret_fs(), inner.inner_inode_num)
        };
        if handle.inode_id() != inode_num {
            // Never attach a capability to the wrong canonical lifetime. The
            // fail-stopped mount retains it for durable orphan recovery.
            fs.fail_stop_lifecycle();
            fs.quarantined_reclaims.lock().push(handle);
            return Err(SystemError::EIO);
        }
        self.defer_reclaim(handle)
    }

    /// Publish the one-shot capability produced by the final unlink. Physical
    /// reclaim waits until every semantic VFS owner has released this inode.
    pub(super) fn defer_reclaim(
        self: &Arc<Self>,
        handle: another_ext4::InodeReclaimHandle,
    ) -> Result<(), SystemError> {
        let mut pending = self.pending_reclaim.lock();
        if pending.is_some() {
            return Err(SystemError::EIO);
        }
        *pending = Some(handle);
        drop(pending);
        self.try_schedule_deferred_eviction()
    }

    fn cancel_deferred_reclaim_after_relink(&self) {
        // Dropping the capability is the in-memory counterpart of the durable
        // orphan-del transaction. A queued eviction, if any, observes None and
        // cleanly aborts instead of treating cancellation as corruption.
        let _ = self.pending_reclaim.lock().take();
    }

    fn try_schedule_deferred_eviction(self: &Arc<Self>) -> Result<(), SystemError> {
        if self.pending_reclaim.lock().is_none() {
            return Ok(());
        }
        let mut scheduled = self.eviction_scheduled.lock();
        if *scheduled {
            return Ok(());
        }
        if self.retention.try_begin_freeing().is_err() {
            return Ok(());
        }
        *scheduled = true;
        let fs = match self.eviction_filesystem.lock().upgrade() {
            Some(fs) => fs,
            None => {
                *scheduled = false;
                self.retention.abort_freeing();
                return Err(SystemError::ESTALE);
            }
        };
        if let Err(error) = fs.schedule_inode_eviction(self.clone()) {
            *scheduled = false;
            self.retention.abort_freeing();
            return Err(error);
        }
        Ok(())
    }

    pub(super) fn run_deferred_eviction(self: &Arc<Self>) -> Result<(), SystemError> {
        let fs = self.inner.lock().concret_fs();
        let tombstone = match fs.begin_freeing(self) {
            Ok(tombstone) => tombstone,
            Err(error) => {
                *self.eviction_scheduled.lock() = false;
                self.retention.abort_freeing();
                return Err(error);
            }
        };
        // Serialize the final capability decision with final unlink/relink.
        // `begin_freeing` first closes operation admission and drains existing
        // operations, so this ordering cannot deadlock a relink that already
        // owns the link-mutation lock.
        let _link_mutation = self.lifecycle().lock_link_mutation();
        let handle = match self.pending_reclaim.lock().take() {
            Some(handle) => handle,
            None => {
                fs.abort_freeing(tombstone)?;
                *self.eviction_scheduled.lock() = false;
                self.retention.abort_freeing();
                return Ok(());
            }
        };
        let _reuse = fs.begin_reclaim();
        if let Some(page_cache) = self.page_cache() {
            if let Err(error) = page_cache.truncate(0) {
                *self.pending_reclaim.lock() = Some(handle);
                let _ = fs.poison_freeing(tombstone, error.clone());
                return Err(error);
            }
        }
        match Self::reclaim_with_metadata_contention_retry(&fs.fs, handle) {
            Ok(()) => {
                fs.complete_freeing(tombstone)?;
                Ok(())
            }
            Err((error, handle)) => {
                *self.pending_reclaim.lock() = Some(handle);
                let error = SystemError::from(error);
                let _ = fs.poison_freeing(tombstone, error.clone());
                Err(error)
            }
        }
    }

    pub(super) fn flush_metadata(&self, datasync: bool) -> Result<(), SystemError> {
        let _operation = self.begin_operation()?;
        let _io_guard = self.io_lock.lock();
        let (
            fs,
            inode_num,
            dirty,
            cached_size,
            cached_times,
            cached_atime_version,
            cached_mtime_version,
        ) = {
            let guard = self.inner.lock();
            (
                guard.concret_fs(),
                guard.inner_inode_num,
                guard.dirty_state,
                guard.cached_file_size,
                guard.cached_times,
                guard.cached_atime_version,
                guard.cached_mtime_version,
            )
        };

        let size_dirty = dirty.contains(InodeDirtyState::SIZE_DIRTY);
        let atime_dirty = dirty.contains(InodeDirtyState::ATIME_DIRTY);
        let mtime_dirty = dirty.contains(InodeDirtyState::MTIME_DIRTY);

        if !size_dirty && (datasync || (!atime_dirty && !mtime_dirty)) {
            self.release_clean_metadata_queue_owner(&fs);
            return Ok(());
        }

        let size = if size_dirty {
            Some(match cached_size {
                Some(size) => size,
                None => fs.fs.getattr(inode_num)?.size,
            })
        } else {
            None
        };
        let atime = if !datasync && atime_dirty {
            Some(cached_times.atime)
        } else {
            None
        };
        let mtime = if !datasync && mtime_dirty {
            Some(cached_times.mtime)
        } else {
            None
        };
        Self::retry_metadata_contention(|| {
            fs.fs.commit_inode_metadata(inode_num, size, atime, mtime)
        })?;

        let mut guard = self.inner.lock();
        if size_dirty && guard.cached_file_size == cached_size {
            guard.dirty_state.remove(InodeDirtyState::SIZE_DIRTY);
        }
        if !datasync && atime_dirty && guard.cached_atime_version == cached_atime_version {
            guard.dirty_state.remove(InodeDirtyState::ATIME_DIRTY);
        }
        if !datasync && mtime_dirty && guard.cached_mtime_version == cached_mtime_version {
            guard.dirty_state.remove(InodeDirtyState::MTIME_DIRTY);
        }
        drop(guard);
        self.release_clean_metadata_queue_owner(&fs);
        Ok(())
    }

    /// Prepare the on-disk extent before a shared file VMA becomes writable.
    ///
    /// This is the ext4 counterpart of Linux `ext4_page_mkwrite()`: page-cache
    /// dirtying alone is insufficient for a sparse page because writeback uses
    /// `write_data_only()` and therefore requires the backing block to exist.
    pub(super) fn prepare_mmap_write(
        &self,
        page_index: usize,
    ) -> Result<Ext4MmapWriteGuard<'_>, SystemError> {
        let operation = self.begin_operation()?;
        let size_guard = self.size_lock.read();
        let io_guard = self.io_lock.lock();
        let (fs, inode_num, file_size) = {
            let mut guard = self.inner.lock();
            let fs = guard.concret_fs();
            let file_size = match guard.cached_file_size {
                Some(size) => size,
                None => {
                    let size = fs.fs.getattr(guard.inner_inode_num)?.size;
                    guard.cached_file_size = Some(size);
                    size
                }
            };
            (fs, guard.inner_inode_num, file_size)
        };
        let page_start = page_index
            .checked_mul(MMArch::PAGE_SIZE)
            .ok_or(SystemError::EFBIG)?;
        if page_start >= file_size as usize {
            return Err(SystemError::EFBIG);
        }
        let time = PosixTimeSpec::now().tv_sec.to_u32().unwrap_or(0);
        Self::retry_metadata_contention(|| {
            fs.fs.prepare_buffered_write(
                inode_num,
                page_start,
                MMArch::PAGE_SIZE,
                file_size,
                Some(time),
            )
        })?;
        // The size read lock remains held through the generic page-cache
        // handoff, so truncate cannot remove the prepared extent.  Release the
        // inode I/O lock first: filemap_page_mkwrite may wait for an existing
        // writeback, whose write_sync path needs this same lock.
        drop(io_guard);

        let self_arc = {
            let mut guard = self.inner.lock();
            guard.cached_times.mtime = time;
            guard.cached_mtime_version = guard.cached_mtime_version.wrapping_add(1);
            guard.self_ref.upgrade().ok_or(SystemError::ENOENT)?
        };
        Ext4FileSystem::mark_inode_dirty(&self_arc, InodeDirtyState::MTIME_DIRTY)?;
        Ok(Ext4MmapWriteGuard {
            _operation: operation,
            _size_guard: size_guard,
        })
    }
}

impl Debug for Ext4Inode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Ext4Inode")
    }
}

pub(crate) fn run_lifecycle_selftests() -> String {
    let mut failures = 0usize;
    let mut report = String::new();
    let mut append = |name: &str, ok: bool| {
        if ok {
            report.push_str(&format!("{name}=ok\n"));
        } else {
            failures += 1;
            report.push_str(&format!("{name}=fail\n"));
        }
    };

    let lifecycle = Ext4InodeLifecycle::new();
    let operation = lifecycle.begin_operation();
    append("live_operation", operation.is_ok());
    drop(operation);
    append("begin_freeing", lifecycle.begin_freeing().is_ok());
    lifecycle.wait_for_quiescent();
    lifecycle.set_state(Ext4InodeLifecycleState::Retired);
    append(
        "retired_rejects_operation",
        lifecycle.begin_operation().err() == Some(SystemError::ESTALE),
    );

    let lifecycle = Ext4InodeLifecycle::new();
    let outer = lifecycle.begin_operation().expect("live operation");
    append("reentrant_begin_freeing", lifecycle.begin_freeing().is_ok());
    let nested = lifecycle.begin_operation();
    append("freeing_allows_owner_nested_operation", nested.is_ok());
    drop(nested);
    drop(outer);
    lifecycle.wait_for_quiescent();
    append(
        "reentrant_operations_drained",
        lifecycle.inner.lock().active_operations == 0,
    );

    let lifecycle = Ext4InodeLifecycle::new();
    append("abort_begin", lifecycle.begin_freeing().is_ok());
    lifecycle.set_state(Ext4InodeLifecycleState::Live);
    append("abort_restores_live", lifecycle.begin_operation().is_ok());

    let lifecycle = Ext4InodeLifecycle::new();
    lifecycle.set_state(Ext4InodeLifecycleState::Poisoned(SystemError::EIO));
    append(
        "poison_is_observable",
        lifecycle.begin_operation().err() == Some(SystemError::EIO),
    );

    let mut attempts = 0usize;
    let retry_result = LockedExt4Inode::retry_metadata_contention(|| {
        attempts += 1;
        if attempts < 3 {
            Err(another_ext4::Ext4Error::new(another_ext4::ErrCode::EAGAIN))
        } else {
            Ok(())
        }
    });
    append(
        "metadata_contention_is_internal",
        retry_result.is_ok() && attempts == 3,
    );

    if failures == 0 {
        report.insert_str(0, "status=ok\n");
    } else {
        report.insert_str(0, &format!("status=fail failures={failures}\n"));
    }
    report
}
