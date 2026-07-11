use crate::{
    arch::MMArch,
    driver::base::device::device_number::{DeviceNumber, Major},
    filesystem::{
        page_cache::{AsyncPageCacheBackend, PageCache},
        vfs::{
            self, syscall::RenameFlags, utils::DName, vcore::generate_inode_id, FilePrivateData,
            IndexNode, InodeFlags, InodeId, InodeMode, SpecialNodeData, XattrFlags,
        },
    },
    ipc::pipe::LockedPipeInode,
    libs::{
        casting::DowncastArc,
        mutex::{Mutex, MutexGuard},
        rwsem::RwSem,
        wait_queue::WaitQueue,
    },
    mm::{truncate::truncate_inode_pages, MemoryManagementArch},
    process::{ProcessManager, RawPid},
    time::PosixTimeSpec,
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
        /// 该 inode 已在文件系统 dirty_inodes 队列中。
        const QUEUED        = 1 << 3;
        /// 该 inode 正在执行元数据写回。
        const WRITEBACK     = 1 << 4;
        /// 仅时间戳脏（lazytime），对应 I_DIRTY_TIME (1 << 11)
        #[allow(dead_code)]
        const TIME_DIRTY    = 1 << 11;
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
    /// 缓存的 mtime。普通 buffered write 先更新内存态，fsync/O_SYNC 再刷到磁盘。
    pub(super) cached_mtime: Option<u32>,
    /// 脏状态标志位，对应 Linux `inode->i_state & I_DIRTY_*`。
    pub(super) dirty_state: InodeDirtyState,
}

#[derive(Debug)]
pub struct LockedExt4Inode(
    pub(super) Mutex<Ext4Inode>,
    pub(super) Mutex<()>,
    pub(super) RwSem<()>,
    pub(super) Mutex<()>,
    pub(super) Arc<Ext4InodeLifecycle>,
);

impl IndexNode for LockedExt4Inode {
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
        let _namespace = self.3.lock();
        let mut guard = self.0.lock();
        // another_ext4的高4位是文件类型，低12位是权限
        let file_mode = InodeMode::from(file_type).union(mode);
        let file_mode = another_ext4::InodeMode::from_bits_truncate(file_mode.bits() as u16);
        let fs = guard.concret_fs();
        let _reuse = fs.begin_allocation()?;
        let ext4 = &fs.fs;

        let id = if file_type == vfs::FileType::Dir {
            ext4.mkdir(guard.inner_inode_num, name, file_mode)?
        } else {
            ext4.create(guard.inner_inode_num, name, file_mode)?
        };

        let dname = DName::from(name);
        // 通过self_ref获取Arc<Self>，然后转换为Arc<dyn IndexNode>
        let self_arc = guard.self_ref.upgrade().ok_or(SystemError::ENOENT)?;
        let inode = fs.publish_allocated_inode(
            id,
            dname.clone(),
            Some(Arc::downgrade(&self_arc)),
            Self::disk_file_type(file_type),
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
            let guard = self.0.lock();
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
            let guard = self.0.lock();
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
        let _size_guard = self.2.read();
        let buf = &buf[0..len];

        let (fs, inode_num, page_cache) = {
            let guard = self.0.lock();
            (
                guard.concret_fs(),
                guard.inner_inode_num,
                guard.page_cache.clone(),
            )
        };

        if let Some(page_cache) = page_cache {
            let _invalidate = page_cache.invalidate_write();
            let _io_guard = self.1.lock();

            // 使用缓存的文件大小，避免 getattr 磁盘 I/O
            let old_file_size = {
                let cached_size = self.0.lock().cached_file_size;
                match cached_size {
                    Some(size) => size,
                    None => {
                        let size = fs.fs.getattr(inode_num)?.size;
                        self.0.lock().cached_file_size = Some(size);
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
            fs.fs
                .prepare_buffered_write(
                    inode_num,
                    alloc_start,
                    alloc_len,
                    new_end as u64,
                    Some(time),
                )
                .map_err(SystemError::from)?;

            // 写入范围的磁盘块已就绪，现在安全写入 page cache。
            let write_len = PageCache::write(&page_cache, offset, buf)?;
            if write_len > 0 {
                let written_end = offset.checked_add(write_len).ok_or(SystemError::EFBIG)?;
                let current_file_size = core::cmp::max(old_file_size, written_end as u64);
                let self_arc = {
                    let mut guard = self.0.lock();
                    guard.cached_file_size = Some(current_file_size);
                    guard.cached_mtime = Some(time);
                    guard.self_ref.upgrade().ok_or(SystemError::ENOENT)?
                };
                Ext4FileSystem::mark_inode_dirty(
                    &self_arc,
                    InodeDirtyState::SIZE_DIRTY | InodeDirtyState::MTIME_DIRTY,
                )?;
            }

            Ok(write_len)
        } else {
            self.write_direct(offset, len, buf, data)
        }
    }

    fn write_sync(&self, offset: usize, buf: &[u8]) -> Result<usize, SystemError> {
        let _operation = self.begin_operation()?;
        let _io_guard = self.1.lock();
        let (fs, inode_num) = {
            let guard = self.0.lock();
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
            FileType::RegularFile => fs
                .fs
                .write_data_only(inode_num, offset, buf)
                .map_err(From::from),
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
        self.0.lock().concret_fs()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn find(&self, name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        let _operation = self.begin_operation()?;
        let _namespace = self.3.lock();
        let mut guard = self.0.lock();
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
        let guard = self.0.lock();

        // 如果存储了父级指针，直接返回
        if let Some(parent) = guard.parent.upgrade() {
            return Ok(parent);
        }

        Err(SystemError::ENOENT)
    }

    fn list(&self) -> Result<Vec<String>, SystemError> {
        let _operation = self.begin_operation()?;
        let guard = self.0.lock();
        let dentry = guard.concret_fs().fs.listdir(guard.inner_inode_num)?;
        let mut list = Vec::new();
        for entry in dentry {
            list.push(entry.name());
        }
        Ok(list)
    }

    fn link(&self, name: &str, other: &Arc<dyn IndexNode>) -> Result<(), SystemError> {
        let _operation = self.begin_operation()?;
        let _namespace = self.3.lock();
        let mut guard = self.0.lock();
        let fs = guard.concret_fs();
        let ext4 = &fs.fs;
        let inode_num = guard.inner_inode_num;

        let other_arc = other
            .clone()
            .downcast_arc::<LockedExt4Inode>()
            .ok_or(SystemError::EINVAL)?;
        let other_fs = other_arc.0.lock().concret_fs();
        if !Arc::ptr_eq(&fs, &other_fs) {
            return Err(SystemError::EXDEV);
        }
        let other_lifecycle = other_arc.lifecycle().clone();
        let _link_mutation = other_lifecycle.lock_link_mutation();
        let _other_operation = other_arc.begin_operation()?;
        let other_inode_num = other_arc.0.lock().inner_inode_num;

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

        ext4.link(other_inode_num, inode_num, name)?;

        let dname = DName::from(name);
        guard.children.insert(dname, other_arc);

        Ok(())
    }

    fn unlink(&self, name: &str) -> Result<(), SystemError> {
        let _operation = self.begin_operation()?;
        let _namespace = self.3.lock();
        let mut guard = self.0.lock();
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
        let target_attr = ext4.getattr(target_num)?;

        // Removing a non-final hard link must not enter the eviction lifecycle: the
        // canonical inode and its shared page cache remain live for the other aliases.
        if target_attr.links > 1 {
            let _target_operation = target.begin_operation()?;
            if ext4.lookup(inode_num, name)? != target_num {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
            ext4.unlink(inode_num, name)?;
            let _ = guard.children.remove(&DName::from(name));
            return Ok(());
        }

        let tombstone = fs.begin_freeing(&target)?;
        match ext4.lookup(inode_num, name) {
            Ok(current) if current == target_num => {}
            Ok(_) => {
                fs.abort_freeing(tombstone)?;
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
            Err(error) => {
                fs.abort_freeing(tombstone)?;
                return Err(error.into());
            }
        }
        let _reuse = fs.begin_reclaim();
        if let Some(page_cache) = target.page_cache() {
            truncate_inode_pages(page_cache, 0);
        }
        let result = ext4.unlink(inode_num, name).map_err(SystemError::from);
        if let Err(error) = result {
            let _ = fs.poison_freeing(tombstone, error.clone());
            return Err(error);
        }
        match ext4.getattr(target_num) {
            Ok(_) => fs.abort_freeing(tombstone)?,
            Err(error) => {
                let error = SystemError::from(error);
                if error == SystemError::EINVAL {
                    fs.complete_freeing(tombstone)?;
                } else {
                    let _ = fs.poison_freeing(tombstone, error.clone());
                    return Err(error);
                }
            }
        }
        // 清理 children 缓存
        let _ = guard.children.remove(&DName::from(name));
        Ok(())
    }

    fn metadata(&self) -> Result<vfs::Metadata, SystemError> {
        let _operation = self.begin_operation()?;
        let (fs, inode_num, vfs_inode_id, cached_size, cached_mtime) = {
            let guard = self.0.lock();
            (
                guard.concret_fs(),
                guard.inner_inode_num,
                guard.vfs_inode_id,
                guard.cached_file_size,
                guard.cached_mtime,
            )
        };
        let attr = fs.fs.getattr(inode_num)?;
        let size = cached_size.unwrap_or(attr.size);
        let mtime = cached_mtime.unwrap_or(attr.mtime);

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
            atime: PosixTimeSpec::new(attr.atime.into(), 0),
            btime: PosixTimeSpec::new(attr.atime.into(), 0),
            mtime: PosixTimeSpec::new(mtime.into(), 0),
            ctime: PosixTimeSpec::new(attr.ctime.into(), 0),
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
        self.flush_metadata(false)
    }

    fn datasync(&self) -> Result<(), SystemError> {
        let _operation = self.begin_operation()?;
        if let Some(page_cache) = self.page_cache() {
            page_cache.manager().sync()?;
        }
        self.flush_metadata(true)
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
        self.flush_metadata(datasync)
    }

    fn write_inode(&self, _wbc: &vfs::WritebackControl) -> Result<(), SystemError> {
        self.flush_metadata(false)
    }

    fn page_cache(&self) -> Option<Arc<PageCache>> {
        self.0.lock().page_cache.clone()
    }

    fn set_metadata(&self, metadata: &vfs::Metadata) -> Result<(), SystemError> {
        let _operation = self.begin_operation()?;
        let mode = metadata.mode.union(InodeMode::from(metadata.file_type));

        let to_ext4_time =
            |time: &PosixTimeSpec| -> u32 { time.tv_sec.max(0).min(u32::MAX as i64) as u32 };

        let (fs, inode_num) = {
            let guard = self.0.lock();
            (guard.concret_fs(), guard.inner_inode_num)
        };
        let ext4 = &fs.fs;
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
        )?;
        {
            let mut guard = self.0.lock();
            guard.cached_file_size = Some(metadata.size as u64);
            guard.cached_mtime = Some(to_ext4_time(&metadata.mtime));
            guard
                .dirty_state
                .remove(InodeDirtyState::SIZE_DIRTY | InodeDirtyState::MTIME_DIRTY);
        }

        Ok(())
    }

    fn resize(&self, len: usize) -> Result<(), SystemError> {
        let _operation = self.begin_operation()?;
        let _size_guard = self.2.write();
        let (fs, inode_num, page_cache, cached_size) = {
            let guard = self.0.lock();
            (
                guard.concret_fs(),
                guard.inner_inode_num,
                guard.page_cache.clone(),
                guard.cached_file_size,
            )
        };
        let old_size = match cached_size {
            Some(size) => size,
            None => fs.fs.getattr(inode_num)?.size,
        };
        {
            let _io_guard = self.1.lock();
            let ext4 = &fs.fs;
            // 仅调整文件大小，其他属性保持不变
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
            .map_err(SystemError::from)?;
            // 更新缓存的文件大小
            {
                let mut guard = self.0.lock();
                guard.cached_file_size = Some(len as u64);
                guard
                    .dirty_state
                    .remove(InodeDirtyState::SIZE_DIRTY | InodeDirtyState::MTIME_DIRTY);
            }
        }
        if len < old_size as usize {
            if let Some(page_cache) = page_cache {
                page_cache.truncate(len)?;
            }
        }
        Ok(())
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
        let _namespace = self.3.lock();
        let mut guard = self.0.lock();
        let fs = guard.concret_fs();
        let concret_fs = &fs.fs;
        let inode_num = guard.inner_inode_num;
        if concret_fs.getattr(inode_num)?.ftype != FileType::Directory {
            return Err(SystemError::ENOTDIR);
        }
        let target_num = concret_fs.lookup(inode_num, name)?;
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
        let tombstone = fs.begin_freeing(&target)?;
        match concret_fs.lookup(inode_num, name) {
            Ok(current) if current == target_num => {}
            Ok(_) => {
                fs.abort_freeing(tombstone)?;
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
            Err(error) => {
                fs.abort_freeing(tombstone)?;
                return Err(error.into());
            }
        }
        let target_attr = match concret_fs.getattr(target_num) {
            Ok(attr) => attr,
            Err(error) => {
                fs.abort_freeing(tombstone)?;
                return Err(error.into());
            }
        };
        if target_attr.ftype != FileType::Directory {
            fs.abort_freeing(tombstone)?;
            return Err(SystemError::ENOTDIR);
        }
        match concret_fs.listdir(target_num) {
            Ok(entries) if entries.len() <= 2 => {}
            Ok(_) => {
                fs.abort_freeing(tombstone)?;
                return Err(SystemError::ENOTEMPTY);
            }
            Err(error) => {
                fs.abort_freeing(tombstone)?;
                return Err(error.into());
            }
        }
        let _reuse = fs.begin_reclaim();
        let result = concret_fs.rmdir(inode_num, name).map_err(SystemError::from);
        if let Err(error) = result {
            let _ = fs.poison_freeing(tombstone, error.clone());
            return Err(error);
        }
        match concret_fs.getattr(target_num) {
            Ok(_) => {
                let error = SystemError::EIO;
                let _ = fs.poison_freeing(tombstone, error.clone());
                return Err(error);
            }
            Err(error) => {
                let error = SystemError::from(error);
                if error == SystemError::EINVAL {
                    fs.complete_freeing(tombstone)?;
                } else {
                    let _ = fs.poison_freeing(tombstone, error.clone());
                    return Err(error);
                }
            }
        }
        // 清理 children 缓存
        let _ = guard.children.remove(&DName::from(name));

        Ok(())
    }

    fn dname(&self) -> Result<DName, SystemError> {
        Ok(self.0.lock().dname.clone())
    }

    fn getxattr(&self, name: &str, buf: &mut [u8]) -> Result<usize, SystemError> {
        let _operation = self.begin_operation()?;
        let guard = self.0.lock();
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
        let guard = self.0.lock();
        let ext4 = &guard.concret_fs().fs;
        let inode_num = guard.inner_inode_num;

        if ext4.getattr(inode_num)?.ftype == FileType::SymLink {
            return Err(SystemError::EPERM);
        }

        ext4.setxattr_with_flags(
            inode_num,
            name,
            value,
            flags.contains(XattrFlags::CREATE),
            flags.contains(XattrFlags::REPLACE),
        )?;

        Ok(0)
    }

    fn listxattr(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        let _operation = self.begin_operation()?;
        let guard = self.0.lock();
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
        let guard = self.0.lock();
        let ext4 = &guard.concret_fs().fs;
        let inode_num = guard.inner_inode_num;

        if ext4.getattr(inode_num)?.ftype == FileType::SymLink {
            return Err(SystemError::EPERM);
        }

        ext4.removexattr(inode_num, name)?;
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
        let _namespace = self.3.lock();

        let mut guard = self.0.lock();
        let fs = guard.concret_fs();
        let _reuse = fs.begin_allocation()?;
        let ext4 = &fs.fs;
        let inode_num = guard.inner_inode_num;

        if ext4.getattr(inode_num)?.ftype != FileType::Directory {
            return Err(SystemError::ENOTDIR);
        }

        // VFS InodeMode(u32) → another_ext4 InodeMode(u16)
        let file_mode = another_ext4::InodeMode::from_bits_truncate(mode.bits() as u16);

        // Create inode based on file type
        let id = if matches!(
            file_type,
            vfs::FileType::CharDevice | vfs::FileType::BlockDevice
        ) {
            // Character/block device: use mknod to store device number in i_block
            ext4.mknod(
                inode_num,
                filename,
                file_mode,
                dev_t.major().data(),
                dev_t.minor(),
            )?
        } else {
            // FIFO, Socket, etc.: use regular create (no device number needed)
            ext4.create(inode_num, filename, file_mode)?
        };

        // Wrap as VFS inode and cache
        let dname = DName::from(filename);
        let self_arc = guard.self_ref.upgrade().ok_or(SystemError::ENOENT)?;
        let inode = fs.publish_allocated_inode(
            id,
            dname.clone(),
            Some(Arc::downgrade(&self_arc)),
            Self::disk_file_type(file_type),
            &_reuse,
        )?;
        guard.children.insert(dname, inode.clone());
        drop(guard);
        Ok(inode as Arc<dyn IndexNode>)
    }

    fn special_node(&self) -> Option<SpecialNodeData> {
        self.0.lock().special_node.clone()
    }

    fn move_to(
        &self,
        old_name: &str,
        target: &Arc<dyn IndexNode>,
        new_name: &str,
        flags: RenameFlags,
    ) -> Result<(), SystemError> {
        let _operation = self.begin_operation()?;
        let target_locked = target
            .clone()
            .downcast_arc::<LockedExt4Inode>()
            .ok_or(SystemError::EXDEV)?;
        let _target_operation = target_locked.begin_operation()?;

        let (ext4_fs, src_inode_num) = {
            let guard = self.0.lock();
            (guard.concret_fs(), guard.inner_inode_num)
        };
        let ext4 = &ext4_fs.fs;
        let target_inode_num = target_locked.0.lock().inner_inode_num;
        if !Arc::ptr_eq(&ext4_fs, &target_locked.0.lock().concret_fs()) {
            return Err(SystemError::EXDEV);
        }

        let (_first_namespace, _second_namespace) = if src_inode_num == target_inode_num {
            (self.3.lock(), None)
        } else if src_inode_num < target_inode_num {
            (self.3.lock(), Some(target_locked.3.lock()))
        } else {
            (target_locked.3.lock(), Some(self.3.lock()))
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
            ext4.rename_exchange(src_inode_num, old_name, target_inode_num, new_name)?;

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
                .0
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
            // Prepare and drain the replacement target before changing the source
            // namespace. No fallible target preparation may occur after exchange.
            let mut tombstone = None;
            let mut will_free = false;
            if let (Some(dst_inode), Some(dst_inode_num)) = (&dst_inode, dst_inode_num) {
                let attr = ext4.getattr(dst_inode_num)?;
                will_free = if attr.ftype == FileType::Directory {
                    attr.links <= 2
                } else {
                    attr.links <= 1
                };
                if will_free {
                    tombstone = Some(ext4_fs.begin_freeing(dst_inode)?);
                    if ext4.lookup(target_inode_num, new_name).ok() != Some(dst_inode_num) {
                        ext4_fs.abort_freeing(tombstone.take().unwrap())?;
                        return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                    }
                    let fresh_attr = match ext4.getattr(dst_inode_num) {
                        Ok(attr) => attr,
                        Err(error) => {
                            ext4_fs.abort_freeing(tombstone.take().unwrap())?;
                            return Err(error.into());
                        }
                    };
                    if fresh_attr.ftype == FileType::Directory {
                        match ext4.listdir(dst_inode_num) {
                            Ok(entries) if entries.len() <= 2 => {}
                            Ok(_) => {
                                ext4_fs.abort_freeing(tombstone.take().unwrap())?;
                                return Err(SystemError::ENOTEMPTY);
                            }
                            Err(error) => {
                                ext4_fs.abort_freeing(tombstone.take().unwrap())?;
                                return Err(error.into());
                            }
                        }
                    }
                }
            }

            let mut temp_name = String::new();
            let mut whiteout_inode = None;
            let source_parent = self
                .0
                .lock()
                .self_ref
                .upgrade()
                .ok_or(SystemError::ENOENT)?;
            for _ in 0..32 {
                let candidate = format!(".dragonos-whiteout-{}", generate_inode_id().data());
                if ext4.lookup(src_inode_num, &candidate).is_ok() {
                    continue;
                }
                let allocation = {
                    match ext4_fs.begin_allocation() {
                        Ok(guard) => guard,
                        Err(error) => {
                            if let Some(tombstone) = tombstone.take() {
                                ext4_fs.abort_freeing(tombstone)?;
                            }
                            return Err(error);
                        }
                    }
                };
                let whiteout_num = match ext4.mknod(
                    src_inode_num,
                    &candidate,
                    another_ext4::InodeMode::CHARDEV
                        | another_ext4::InodeMode::from_bits_retain(0o600),
                    WHITEOUT_DEV.major().data(),
                    WHITEOUT_DEV.minor(),
                ) {
                    Ok(inode_num) => inode_num,
                    Err(error) => {
                        if let Some(tombstone) = tombstone.take() {
                            ext4_fs.abort_freeing(tombstone)?;
                        }
                        return Err(error.into());
                    }
                };
                whiteout_inode = match ext4_fs.publish_allocated_inode(
                    whiteout_num,
                    DName::from(candidate.as_str()),
                    Some(Arc::downgrade(&source_parent)),
                    FileType::CharacterDev,
                    &allocation,
                ) {
                    Ok(inode) => Some(inode),
                    Err(error) => {
                        drop(allocation);
                        let _reclaim = ext4_fs.begin_reclaim();
                        if ext4.unlink(src_inode_num, &candidate).is_err() {
                            ext4_fs.fail_stop_lifecycle();
                            if let Some(tombstone) = tombstone.take() {
                                let _ = ext4_fs.poison_freeing(tombstone, SystemError::EIO);
                            }
                            return Err(SystemError::EIO);
                        }
                        if let Some(tombstone) = tombstone.take() {
                            ext4_fs.abort_freeing(tombstone)?;
                        }
                        return Err(error);
                    }
                };
                temp_name = candidate;
                break;
            }
            if temp_name.is_empty() {
                if let Some(tombstone) = tombstone.take() {
                    ext4_fs.abort_freeing(tombstone)?;
                }
                return Err(SystemError::EEXIST);
            }

            if let Err(err) =
                ext4.rename_exchange(src_inode_num, old_name, src_inode_num, &temp_name)
            {
                if let Some(tombstone) = tombstone.take() {
                    ext4_fs.abort_freeing(tombstone)?;
                }
                Self::reclaim_temporary_inode(
                    &ext4_fs,
                    src_inode_num,
                    &temp_name,
                    whiteout_inode.take().unwrap(),
                )?;
                return Err(err.into());
            }
            let _reuse = tombstone.as_ref().map(|_| ext4_fs.begin_reclaim());
            if will_free {
                if let Some(pc) = dst_inode.as_ref().and_then(|inode| inode.page_cache()) {
                    truncate_inode_pages(pc, 0);
                }
            }
            if let Err(err) = ext4.rename(src_inode_num, &temp_name, target_inode_num, new_name) {
                let rename_error = SystemError::from(err);
                let rollback =
                    ext4.rename_exchange(src_inode_num, old_name, src_inode_num, &temp_name);
                if let Some(tombstone) = tombstone.take() {
                    let _ = ext4_fs.poison_freeing(tombstone, rename_error.clone());
                }
                drop(_reuse);
                if rollback.is_err() {
                    let whiteout_tombstone = ext4_fs
                        .begin_freeing(whiteout_inode.as_ref().expect("whiteout was published"))?;
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
            if let (Some(dst_inode_num), Some(tombstone)) = (dst_inode_num, tombstone.take()) {
                match ext4.getattr(dst_inode_num) {
                    Ok(_) => ext4_fs.abort_freeing(tombstone)?,
                    Err(error) => {
                        let error = SystemError::from(error);
                        if error == SystemError::EINVAL {
                            ext4_fs.complete_freeing(tombstone)?;
                        } else {
                            let _ = ext4_fs.poison_freeing(tombstone, error.clone());
                            return Err(error);
                        }
                    }
                }
            }
            if let Some(whiteout) = &whiteout_inode {
                whiteout.0.lock().dname = old_dname.clone();
            }
            resulting_whiteout = whiteout_inode;
        } else {
            if let Some(dst_inode) = &dst_inode {
                let attr = ext4.getattr(dst_inode_num.unwrap())?;
                let will_free = if attr.ftype == FileType::Directory {
                    attr.links <= 2
                } else {
                    attr.links <= 1
                };
                if !will_free {
                    ext4.rename(src_inode_num, old_name, target_inode_num, new_name)?;
                } else {
                    let tombstone = ext4_fs.begin_freeing(dst_inode)?;
                    if ext4.lookup(target_inode_num, new_name).ok() != dst_inode_num {
                        ext4_fs.abort_freeing(tombstone)?;
                        return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                    }
                    let fresh_attr = match ext4.getattr(dst_inode_num.unwrap()) {
                        Ok(attr) => attr,
                        Err(error) => {
                            ext4_fs.abort_freeing(tombstone)?;
                            return Err(error.into());
                        }
                    };
                    if fresh_attr.ftype == FileType::Directory {
                        match ext4.listdir(dst_inode_num.unwrap()) {
                            Ok(entries) if entries.len() <= 2 => {}
                            Ok(_) => {
                                ext4_fs.abort_freeing(tombstone)?;
                                return Err(SystemError::ENOTEMPTY);
                            }
                            Err(error) => {
                                ext4_fs.abort_freeing(tombstone)?;
                                return Err(error.into());
                            }
                        }
                    }
                    let _reuse = ext4_fs.begin_reclaim();
                    if let Some(pc) = dst_inode.page_cache() {
                        truncate_inode_pages(pc, 0);
                    }
                    if let Err(err) =
                        ext4.rename(src_inode_num, old_name, target_inode_num, new_name)
                    {
                        let error = SystemError::from(err);
                        let _ = ext4_fs.poison_freeing(tombstone, error.clone());
                        return Err(error);
                    }
                    match ext4.getattr(dst_inode_num.unwrap()) {
                        Ok(_) => ext4_fs.abort_freeing(tombstone)?,
                        Err(error) => {
                            let error = SystemError::from(error);
                            if error == SystemError::EINVAL {
                                ext4_fs.complete_freeing(tombstone)?;
                            } else {
                                let _ = ext4_fs.poison_freeing(tombstone, error.clone());
                                return Err(error);
                            }
                        }
                    }
                }
            } else {
                // ext4 library now correctly handles atomic replace
                ext4.rename(src_inode_num, old_name, target_inode_num, new_name)?;
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
            self.0.lock().children.insert(old_dname, whiteout);
        }
        Ok(())
    }
}

impl LockedExt4Inode {
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
        if let Err(error) = fs.fs.unlink(parent_inode_num, name) {
            let error = SystemError::from(error);
            let _ = fs.poison_freeing(tombstone, error.clone());
            return Err(error);
        }
        let inode_num = inode.0.lock().inner_inode_num;
        match fs.fs.getattr(inode_num) {
            Ok(_) => {
                let error = SystemError::EIO;
                let _ = fs.poison_freeing(tombstone, error.clone());
                Err(error)
            }
            Err(error) => {
                let error = SystemError::from(error);
                if error == SystemError::EINVAL {
                    fs.complete_freeing(tombstone)
                } else {
                    let _ = fs.poison_freeing(tombstone, error.clone());
                    Err(error)
                }
            }
        }
    }

    fn disk_file_type(file_type: vfs::FileType) -> FileType {
        match file_type {
            vfs::FileType::Dir => FileType::Directory,
            vfs::FileType::BlockDevice => FileType::BlockDev,
            vfs::FileType::CharDevice
            | vfs::FileType::FramebufferDevice
            | vfs::FileType::KvmDevice => FileType::CharacterDev,
            vfs::FileType::Pipe => FileType::Fifo,
            vfs::FileType::SymLink => FileType::SymLink,
            vfs::FileType::Socket => FileType::Socket,
            vfs::FileType::File => FileType::RegularFile,
        }
    }

    #[inline]
    fn begin_operation(&self) -> Result<Ext4InodeOperation, SystemError> {
        self.4.begin_operation()
    }

    pub(super) fn lifecycle(&self) -> &Arc<Ext4InodeLifecycle> {
        &self.4
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
            let mut guard = self.0.lock();
            if had_dst {
                guard.children.remove(new_dname);
            }
            if let Some(child) = guard.children.remove(old_dname) {
                child.0.lock().dname = new_dname.clone();
                guard.children.insert(new_dname.clone(), child);
            }
        } else {
            let (mut src_guard, mut dst_guard) = if src_dir < dst_dir {
                (self.0.lock(), target.0.lock())
            } else {
                let d = target.0.lock();
                let s = self.0.lock();
                (s, d)
            };

            if had_dst {
                dst_guard.children.remove(new_dname);
            }
            if let Some(child) = src_guard.children.remove(old_dname) {
                dst_guard.children.insert(new_dname.clone(), child.clone());
                drop(src_guard);
                drop(dst_guard);
                let mut child_guard = child.0.lock();
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
            let mut guard = self.0.lock();
            let old_child = guard.children.remove(old_dname);
            let new_child = guard.children.remove(new_dname);

            if let Some(child) = old_child {
                child.0.lock().dname = new_dname.clone();
                guard.children.insert(new_dname.clone(), child);
            }
            if let Some(child) = new_child {
                child.0.lock().dname = old_dname.clone();
                guard.children.insert(old_dname.clone(), child);
            }
        } else {
            // 跨目录交换
            let (mut src_guard, mut dst_guard) = if src_dir < dst_dir {
                (self.0.lock(), target.0.lock())
            } else {
                let d = target.0.lock();
                let s = self.0.lock();
                (s, d)
            };

            let old_child = src_guard.children.remove(old_dname);
            let new_child = dst_guard.children.remove(new_dname);

            // old_child 移到 target 目录
            if let Some(child) = old_child {
                dst_guard.children.insert(new_dname.clone(), child.clone());
                drop(src_guard);
                drop(dst_guard);

                let mut child_guard = child.0.lock();
                child_guard.dname = new_dname.clone();
                child_guard.parent = Arc::downgrade(target);
                drop(child_guard);

                // 重新获取锁处理 new_child
                if let Some(new_c) = new_child {
                    let mut src_guard = self.0.lock();
                    src_guard.children.insert(old_dname.clone(), new_c.clone());
                    drop(src_guard);

                    let mut new_c_guard = new_c.0.lock();
                    new_c_guard.dname = old_dname.clone();
                    new_c_guard.parent = self.0.lock().self_ref.clone();
                }
            } else if let Some(new_c) = new_child {
                // 只有 new_child 在缓存中
                src_guard.children.insert(old_dname.clone(), new_c.clone());
                drop(src_guard);
                drop(dst_guard);

                let mut new_c_guard = new_c.0.lock();
                new_c_guard.dname = old_dname.clone();
                new_c_guard.parent = self.0.lock().self_ref.clone();
            }
        }
    }

    pub fn new(
        inode_num: u32,
        fs_ptr: Weak<super::filesystem::Ext4FileSystem>,
        dname: DName,
        parent: Option<Weak<LockedExt4Inode>>,
        known_file_type: Option<FileType>,
    ) -> Result<Arc<Self>, SystemError> {
        let lifecycle = Ext4InodeLifecycle::new();
        let inode = Arc::new({
            LockedExt4Inode(
                Mutex::new(Ext4Inode::new(inode_num, fs_ptr.clone(), dname, parent)),
                Mutex::new(()),
                RwSem::new(()),
                Mutex::new(()),
                lifecycle,
            )
        });
        let mut guard = inode.0.lock();

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
        if let Some(fs) = fs_ptr.upgrade() {
            let file_type = match known_file_type {
                Some(file_type) => file_type,
                None => fs.fs.getattr(inode_num)?.ftype,
            };
            if file_type == FileType::Fifo {
                let pipe_inode = LockedPipeInode::new();
                pipe_inode.set_fifo();
                guard.special_node = Some(SpecialNodeData::Pipe(pipe_inode));
            }
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

    pub fn new(
        inode_num: u32,
        fs_ptr: Weak<Ext4FileSystem>,
        dname: DName,
        parent: Option<Weak<LockedExt4Inode>>,
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
            cached_mtime: None,
            dirty_state: InodeDirtyState::empty(),
        }
    }
}

impl LockedExt4Inode {
    pub(super) fn flush_metadata(&self, datasync: bool) -> Result<(), SystemError> {
        let _operation = self.begin_operation()?;
        let _io_guard = self.1.lock();
        let (fs, inode_num, dirty, cached_size, cached_mtime) = {
            let guard = self.0.lock();
            (
                guard.concret_fs(),
                guard.inner_inode_num,
                guard.dirty_state,
                guard.cached_file_size,
                guard.cached_mtime,
            )
        };

        let size_dirty = dirty.contains(InodeDirtyState::SIZE_DIRTY);
        let mtime_dirty = dirty.contains(InodeDirtyState::MTIME_DIRTY);

        if !size_dirty && (!mtime_dirty || datasync) {
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
        let mtime = if !datasync && mtime_dirty {
            cached_mtime
        } else {
            None
        };
        fs.fs
            .commit_inode_metadata(inode_num, size, mtime)
            .map_err(SystemError::from)?;

        let mut guard = self.0.lock();
        if size_dirty && guard.cached_file_size == cached_size {
            guard.dirty_state.remove(InodeDirtyState::SIZE_DIRTY);
        }
        if !datasync && mtime_dirty && guard.cached_mtime == cached_mtime {
            guard.dirty_state.remove(InodeDirtyState::MTIME_DIRTY);
        }
        Ok(())
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

    if failures == 0 {
        report.insert_str(0, "status=ok\n");
    } else {
        report.insert_str(0, &format!("status=fail failures={failures}\n"));
    }
    report
}
