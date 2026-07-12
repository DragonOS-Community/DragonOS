use crate::{
    driver::base::{block::gendisk::GenDisk, device::device_number::DeviceNumber},
    exception::workqueue::{schedule_work, Work},
    filesystem::{
        ext4::inode::{Ext4Inode, Ext4InodeLifecycle, Ext4InodeLifecycleState, InodeDirtyState},
        vfs::{
            self,
            fcntl::AtFlags,
            utils::{user_path_at, DName},
            vcore::{generate_inode_id, try_find_gendisk},
            EvictionEpoch, FileSystem, FileSystemMakerData, IndexNode, Magic, MountableFileSystem,
            FSMAKER, VFS_MAX_FOLLOW_SYMLINK_TIMES,
        },
    },
    libs::{
        mutex::Mutex,
        rwsem::{RwSem, RwSemReadGuard, RwSemWriteGuard},
        spinlock::SpinLock,
        wait_queue::WaitQueue,
    },
    mm::{
        fault::{PageFaultHandler, PageFaultMessage},
        VmFaultReason,
    },
    process::ProcessManager,
    register_mountable_fs,
};
use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
    vec::Vec,
};
use kdepends::another_ext4;
use linkme::distributed_slice;
use system_error::SystemError;

use super::inode::LockedExt4Inode;

#[derive(Debug)]
struct CanonicalInodeEntry {
    inode: Weak<LockedExt4Inode>,
    lifecycle: Arc<Ext4InodeLifecycle>,
}

#[derive(Debug, Default)]
struct Ext4EvictionQueueState {
    next_epoch: u64,
    completed_epoch: u64,
    sealed: bool,
    error: Option<SystemError>,
}

#[must_use]
pub(super) struct Ext4InodeTombstone {
    fs: Weak<Ext4FileSystem>,
    inode_num: u32,
    lifecycle: Arc<Ext4InodeLifecycle>,
    resolved: bool,
}

pub struct Ext4FileSystem {
    /// 对应 another_ext4 中的实际文件系统
    pub(super) fs: another_ext4::Ext4,
    /// 当前文件系统对应的设备号
    pub(super) raw_dev: DeviceNumber,

    /// 根 inode
    root_inode: Arc<LockedExt4Inode>,

    /// 元数据（size/mtime）脏但尚未刷盘的 inode 列表。
    dirty_inodes: Mutex<Vec<Arc<LockedExt4Inode>>>,

    /// Per-superblock canonical VFS inode identity, keyed by the on-disk inode number.
    inode_table: Mutex<BTreeMap<u32, CanonicalInodeEntry>>,

    /// Allocations hold a read guard through canonical publication. Physical reclaim
    /// holds a write guard through tombstone completion so an inode number cannot be
    /// reused in the handoff window.
    inode_reuse_barrier: RwSem<()>,

    /// Fail-stop allocation after an indeterminate physical reclaim.
    lifecycle_error: Mutex<Option<SystemError>>,

    eviction_queue: SpinLock<Ext4EvictionQueueState>,
    eviction_wait: WaitQueue,

    /// Mount-time ext4 options parsed from user/kernel mount data.
    _mount_options: Ext4MountOptions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ext4DaxMode {
    Never,
    Always,
    Inode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ext4ErrorsBehavior {
    Continue,
    RemountRo,
    Panic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ext4MountOptions {
    pub dax: Option<Ext4DaxMode>,
    pub errors: Ext4ErrorsBehavior,
}

impl Default for Ext4MountOptions {
    fn default() -> Self {
        Self {
            dax: None,
            errors: Ext4ErrorsBehavior::Continue,
        }
    }
}

impl FileSystem for Ext4FileSystem {
    fn supports_reliable_flush(&self) -> bool {
        self.fs.supports_reliable_flush()
    }

    fn root_inode(&self) -> Arc<dyn IndexNode> {
        self.root_inode.clone()
    }

    fn info(&self) -> vfs::FsInfo {
        todo!()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn name(&self) -> &str {
        "ext4"
    }

    fn super_block(&self) -> vfs::SuperBlock {
        vfs::SuperBlock::new(Magic::EXT4_MAGIC, another_ext4::BLOCK_SIZE as u64, 255)
    }

    fn statfs(&self, _inode: &Arc<dyn IndexNode>) -> Result<vfs::SuperBlock, SystemError> {
        self.read_statfs_from_superblock()
    }

    unsafe fn fault(&self, pfm: &mut PageFaultMessage) -> VmFaultReason {
        PageFaultHandler::filemap_fault(pfm)
    }

    unsafe fn page_mkwrite(&self, pfm: &mut PageFaultMessage) -> VmFaultReason {
        PageFaultHandler::filemap_page_mkwrite(pfm)
    }

    unsafe fn map_pages(
        &self,
        pfm: &mut PageFaultMessage,
        start_pgoff: usize,
        end_pgoff: usize,
    ) -> VmFaultReason {
        PageFaultHandler::filemap_map_pages(pfm, start_pgoff, end_pgoff)
    }

    fn sync_fs(&self, wait: bool) -> Result<(), SystemError> {
        let eviction_epoch = wait.then(|| {
            // Like Linux ext4 flushing its filesystem workqueue before waiting
            // for the target transaction, include every reclaim request that
            // was published before this sync.  Do not seal the queue: requests
            // published after this snapshot belong to a later sync boundary.
            EvictionEpoch::new(self.eviction_queue.lock().next_epoch)
        });
        let flush_result = self.flush_dirty_inodes();
        let result = if let Some(epoch) = eviction_epoch {
            // Finish the snapshotted asynchronous metadata work even if inode
            // writeback failed, while preserving that earlier error for the
            // caller and the superblock writeback error sequence.
            let eviction_result = self.drain_evictions_through(epoch);
            flush_result.and(eviction_result)
        } else {
            flush_result
        };
        if wait {
            result.and_then(|_| self.fs.flush_device().map_err(SystemError::from))
        } else {
            result
        }
    }

    fn on_umount(&self) {
        if let Err(error) = self.fs.shutdown_writable() {
            log::error!("ext4: failed to mark journal clean on unmount: {:?}", error);
        }
    }

    fn seal_eviction_queue(&self) -> EvictionEpoch {
        let mut queue = self.eviction_queue.lock();
        queue.sealed = true;
        EvictionEpoch::new(queue.next_epoch)
    }

    fn drain_evictions_through(&self, epoch: EvictionEpoch) -> Result<(), SystemError> {
        self.eviction_wait
            .wait_until(|| {
                let queue = self.eviction_queue.lock();
                (queue.completed_epoch >= epoch.value()).then(|| queue.error.clone())
            })
            .map_or(Ok(()), Err)
    }
}

impl Ext4FileSystem {
    pub(super) fn schedule_inode_eviction(
        self: &Arc<Self>,
        inode: Arc<LockedExt4Inode>,
    ) -> Result<(), SystemError> {
        let fs = self.clone();
        {
            // Linearize epoch allocation with FIFO publication so concurrent
            // producers cannot invert completion epochs.
            let mut queue = self.eviction_queue.lock();
            if queue.sealed {
                return Err(SystemError::EBUSY);
            }
            queue.next_epoch = queue
                .next_epoch
                .checked_add(1)
                .ok_or(SystemError::EOVERFLOW)?;
            let epoch = queue.next_epoch;
            schedule_work(Work::new(move || {
                let result = inode.run_deferred_eviction();
                let mut queue = fs.eviction_queue.lock();
                if let Err(error) = result {
                    queue.error.get_or_insert(error);
                }
                queue.completed_epoch = epoch;
                drop(queue);
                fs.eviction_wait.wake_all();
            }));
        }
        Ok(())
    }

    pub(super) fn begin_allocation(&self) -> Result<RwSemReadGuard<'_, ()>, SystemError> {
        let guard = self.inode_reuse_barrier.read();
        if let Some(error) = self.lifecycle_error.lock().clone() {
            return Err(error);
        }
        Ok(guard)
    }

    pub(super) fn begin_reclaim(&self) -> RwSemWriteGuard<'_, ()> {
        self.inode_reuse_barrier.write()
    }

    pub(super) fn fail_stop_lifecycle(&self) {
        *self.lifecycle_error.lock() = Some(SystemError::EIO);
    }

    pub(super) fn get_or_create_inode(
        self: &Arc<Self>,
        inode_num: u32,
        dname: DName,
        parent: Option<Weak<LockedExt4Inode>>,
    ) -> Result<Arc<LockedExt4Inode>, SystemError> {
        self.get_or_create_inode_inner(inode_num, dname, parent, false, None)
    }

    pub(super) fn publish_allocated_inode(
        self: &Arc<Self>,
        inode_num: u32,
        dname: DName,
        parent: Option<Weak<LockedExt4Inode>>,
        file_type: another_ext4::FileType,
        _reuse_guard: &RwSemReadGuard<'_, ()>,
    ) -> Result<Arc<LockedExt4Inode>, SystemError> {
        self.get_or_create_inode_inner(inode_num, dname, parent, true, Some(file_type))
    }

    fn get_or_create_inode_inner(
        self: &Arc<Self>,
        inode_num: u32,
        dname: DName,
        parent: Option<Weak<LockedExt4Inode>>,
        reuse_guard_held: bool,
        known_file_type: Option<another_ext4::FileType>,
    ) -> Result<Arc<LockedExt4Inode>, SystemError> {
        let mut candidate = None;
        let mut admission_guard = None;
        loop {
            let wait_lifecycle = {
                let mut table = self.inode_table.lock();
                match table.get(&inode_num) {
                    Some(entry) => match entry.lifecycle.state() {
                        Ext4InodeLifecycleState::Live => {
                            if let Some(inode) = entry.inode.upgrade() {
                                return Ok(inode);
                            }
                            entry.lifecycle.set_state(Ext4InodeLifecycleState::Retired);
                            table.remove(&inode_num);
                            None
                        }
                        Ext4InodeLifecycleState::Freeing => {
                            candidate = None;
                            drop(admission_guard.take());
                            Some(entry.lifecycle.clone())
                        }
                        Ext4InodeLifecycleState::Retired => {
                            candidate = None;
                            drop(admission_guard.take());
                            table.remove(&inode_num);
                            None
                        }
                        Ext4InodeLifecycleState::Poisoned(error) => return Err(error),
                    },
                    None => {
                        if candidate.is_none() {
                            drop(table);
                            if !reuse_guard_held {
                                admission_guard = Some(self.inode_reuse_barrier.read());
                            }
                            candidate = Some(LockedExt4Inode::new(
                                inode_num,
                                Arc::downgrade(self),
                                dname.clone(),
                                parent.clone(),
                                known_file_type,
                            )?);
                            continue;
                        }
                        let inode = candidate.take().expect("candidate checked above");
                        table.insert(
                            inode_num,
                            CanonicalInodeEntry {
                                inode: Arc::downgrade(&inode),
                                lifecycle: inode.lifecycle().clone(),
                            },
                        );
                        drop(admission_guard.take());
                        return Ok(inode);
                    }
                }
            };

            if let Some(lifecycle) = wait_lifecycle {
                match lifecycle.wait_while_freeing() {
                    Ext4InodeLifecycleState::Poisoned(error) => return Err(error),
                    _ => continue,
                }
            }
        }
    }

    pub(super) fn validate_inode(&self, inode: &Arc<LockedExt4Inode>) -> Result<(), SystemError> {
        let inode_num = inode.0.lock().inner_inode_num;
        let table = self.inode_table.lock();
        let entry = table.get(&inode_num).ok_or(SystemError::ESTALE)?;
        if !Weak::ptr_eq(&entry.inode, &Arc::downgrade(inode))
            || !Arc::ptr_eq(&entry.lifecycle, inode.lifecycle())
        {
            return Err(SystemError::ESTALE);
        }
        inode.lifecycle().begin_operation().map(drop)
    }

    pub(super) fn begin_freeing(
        self: &Arc<Self>,
        inode: &Arc<LockedExt4Inode>,
    ) -> Result<Ext4InodeTombstone, SystemError> {
        let inode_num = inode.0.lock().inner_inode_num;
        {
            let table = self.inode_table.lock();
            let entry = table.get(&inode_num).ok_or(SystemError::ESTALE)?;
            if !Weak::ptr_eq(&entry.inode, &Arc::downgrade(inode))
                || !Arc::ptr_eq(&entry.lifecycle, inode.lifecycle())
            {
                return Err(SystemError::ESTALE);
            }
            entry.lifecycle.begin_freeing()?;
        }
        inode.lifecycle().wait_for_quiescent();
        Ok(Ext4InodeTombstone {
            fs: Arc::downgrade(self),
            inode_num,
            lifecycle: inode.lifecycle().clone(),
            resolved: false,
        })
    }

    fn finish_tombstone(
        &self,
        tombstone: &mut Ext4InodeTombstone,
        state: Ext4InodeLifecycleState,
        remove: bool,
    ) -> Result<(), SystemError> {
        let mut table = self.inode_table.lock();
        let entry = table.get(&tombstone.inode_num).ok_or(SystemError::ESTALE)?;
        if !Arc::ptr_eq(&entry.lifecycle, &tombstone.lifecycle) {
            return Err(SystemError::ESTALE);
        }
        tombstone.lifecycle.set_state(state);
        if remove {
            table.remove(&tombstone.inode_num);
        }
        tombstone.resolved = true;
        Ok(())
    }

    pub(super) fn complete_freeing(
        &self,
        mut tombstone: Ext4InodeTombstone,
    ) -> Result<(), SystemError> {
        self.finish_tombstone(&mut tombstone, Ext4InodeLifecycleState::Retired, true)
    }

    pub(super) fn abort_freeing(
        &self,
        mut tombstone: Ext4InodeTombstone,
    ) -> Result<(), SystemError> {
        self.finish_tombstone(&mut tombstone, Ext4InodeLifecycleState::Live, false)
    }

    pub(super) fn poison_freeing(
        &self,
        mut tombstone: Ext4InodeTombstone,
        _error: SystemError,
    ) -> Result<(), SystemError> {
        let poison = SystemError::EIO;
        *self.lifecycle_error.lock() = Some(poison.clone());
        self.finish_tombstone(
            &mut tombstone,
            Ext4InodeLifecycleState::Poisoned(poison),
            false,
        )
    }

    pub(super) fn mark_inode_dirty(
        inode: &Arc<LockedExt4Inode>,
        dirty: InodeDirtyState,
    ) -> Result<(), SystemError> {
        let _operation = inode.lifecycle().begin_operation()?;
        let (fs, should_queue) = {
            let mut guard = inode.0.lock();
            guard.dirty_state.insert(dirty);
            let should_queue = !guard
                .dirty_state
                .intersects(InodeDirtyState::QUEUED | InodeDirtyState::WRITEBACK);
            if should_queue {
                guard.dirty_state.insert(InodeDirtyState::QUEUED);
            }
            (guard.fs_ptr.upgrade(), should_queue)
        };

        if should_queue {
            if let Some(fs) = fs {
                fs.dirty_inodes.lock().push(inode.clone());
            }
        }
        Ok(())
    }

    fn flush_dirty_inodes(&self) -> Result<(), SystemError> {
        let dirty: Vec<Arc<LockedExt4Inode>> = {
            let mut guard = self.dirty_inodes.lock();
            if guard.is_empty() {
                return Ok(());
            }
            core::mem::take(&mut *guard)
        };

        let mut last_err = Ok(());
        let mut requeue: Vec<Arc<LockedExt4Inode>> = Vec::new();
        for inode in dirty {
            let mut should_requeue = false;
            let result = {
                let has_dirty_metadata = {
                    let mut guard = inode.0.lock();
                    let has_dirty = guard
                        .dirty_state
                        .intersects(InodeDirtyState::SIZE_DIRTY | InodeDirtyState::MTIME_DIRTY);
                    if !has_dirty {
                        guard.dirty_state.remove(InodeDirtyState::QUEUED);
                    }
                    has_dirty
                };
                if !has_dirty_metadata {
                    continue;
                }
                let _operation = match inode.lifecycle().begin_operation() {
                    Ok(operation) => operation,
                    Err(error @ (SystemError::ESTALE | SystemError::EIO)) => {
                        log::error!(
                            "ext4: rejecting stale metadata writeback before disk access: {:?}",
                            error
                        );
                        if let Some(page_cache) = inode.0.lock().page_cache.clone() {
                            page_cache.record_writeback_error_with_superblock(error);
                        }
                        continue;
                    }
                    Err(error) => {
                        requeue.push(inode);
                        last_err = Err(error);
                        continue;
                    }
                };
                let _io_guard = inode.1.lock();
                let (fs, inode_num, snapshot_dirty, cached_size, cached_mtime) = {
                    let mut guard = inode.0.lock();
                    guard.dirty_state.remove(InodeDirtyState::QUEUED);
                    let snapshot_dirty = guard
                        .dirty_state
                        .intersection(InodeDirtyState::SIZE_DIRTY | InodeDirtyState::MTIME_DIRTY);
                    if snapshot_dirty.is_empty() {
                        guard.dirty_state.remove(InodeDirtyState::WRITEBACK);
                        continue;
                    }
                    guard.dirty_state.insert(InodeDirtyState::WRITEBACK);
                    (
                        guard.fs_ptr.upgrade(),
                        guard.inner_inode_num,
                        snapshot_dirty,
                        guard.cached_file_size,
                        guard.cached_mtime,
                    )
                };

                let result = if let Some(fs) = fs {
                    let size = if snapshot_dirty.contains(InodeDirtyState::SIZE_DIRTY) {
                        match cached_size {
                            Some(size) => Ok(Some(size)),
                            None => fs
                                .fs
                                .getattr(inode_num)
                                .map(|attr| Some(attr.size))
                                .map_err(SystemError::from),
                        }
                    } else {
                        Ok(None)
                    };
                    size.and_then(|size| {
                        let mtime = if snapshot_dirty.contains(InodeDirtyState::MTIME_DIRTY) {
                            cached_mtime
                        } else {
                            None
                        };
                        fs.fs
                            .commit_inode_metadata(inode_num, size, mtime)
                            .map_err(SystemError::from)
                    })
                } else {
                    Err(SystemError::EIO)
                };

                let mut guard = inode.0.lock();
                if result.is_ok() {
                    if snapshot_dirty.contains(InodeDirtyState::SIZE_DIRTY)
                        && guard.cached_file_size == cached_size
                    {
                        guard.dirty_state.remove(InodeDirtyState::SIZE_DIRTY);
                    }
                    if snapshot_dirty.contains(InodeDirtyState::MTIME_DIRTY)
                        && guard.cached_mtime == cached_mtime
                    {
                        guard.dirty_state.remove(InodeDirtyState::MTIME_DIRTY);
                    }
                }
                guard.dirty_state.remove(InodeDirtyState::WRITEBACK);
                if guard
                    .dirty_state
                    .intersects(InodeDirtyState::SIZE_DIRTY | InodeDirtyState::MTIME_DIRTY)
                {
                    guard.dirty_state.insert(InodeDirtyState::QUEUED);
                    should_requeue = true;
                }
                result
            };

            if let Err(e) = result {
                log::warn!("flush_dirty_inodes: 元数据刷盘失败: {:?}", e);
                last_err = Err(e);
            }
            if should_requeue {
                requeue.push(inode);
            }
        }
        if !requeue.is_empty() {
            self.dirty_inodes.lock().extend(requeue);
        }
        last_err
    }

    fn read_statfs_from_superblock(&self) -> Result<vfs::SuperBlock, SystemError> {
        let ext4_sb = self.fs.super_block()?;
        let block_size = ext4_sb.block_size();
        let blocks = ext4_sb.block_count();
        let overhead_blocks = ext4_sb.clusters_to_blocks(ext4_sb.overhead_clusters() as u64);
        let bfree = ext4_sb.free_blocks_count();
        let reserved = ext4_sb.reserved_blocks_count();

        let mut sb = vfs::SuperBlock::new(Magic::EXT4_MAGIC, block_size, 255);
        // Linux ext4 语义：f_blocks 不包含元数据开销。
        sb.blocks = blocks.saturating_sub(overhead_blocks);
        sb.bfree = bfree;
        sb.bavail = bfree.saturating_sub(reserved);
        sb.files = ext4_sb.inode_count() as u64;
        sb.ffree = ext4_sb.free_inodes_count() as u64;
        sb.frsize = block_size;
        Ok(sb)
    }

    /// 探测 gendisk 是否包含 ext4 文件系统
    pub fn probe(gendisk: &Arc<GenDisk>) -> Result<bool, SystemError> {
        Ok(another_ext4::Ext4::load(gendisk.clone())
            .map(|_| true)
            .unwrap_or(false))
    }

    pub fn from_gendisk(mount_data: Arc<GenDisk>) -> Result<Arc<dyn FileSystem>, SystemError> {
        Self::from_gendisk_with_options(mount_data, Ext4MountOptions::default())
    }

    pub fn from_gendisk_with_options(
        mount_data: Arc<GenDisk>,
        mount_options: Ext4MountOptions,
    ) -> Result<Arc<dyn FileSystem>, SystemError> {
        let raw_dev = mount_data.device_num();
        // Writable mounts recover the journal and the validated legacy orphan
        // chain before this filesystem is published to the VFS.
        let fs = another_ext4::Ext4::load_writable(mount_data.clone())?;
        let root_inode: Arc<LockedExt4Inode> =
            Arc::new_cyclic(|self_ref: &Weak<LockedExt4Inode>| {
                LockedExt4Inode(
                    Mutex::new(Ext4Inode {
                        inner_inode_num: another_ext4::EXT4_ROOT_INO,
                        fs_ptr: Weak::default(),
                        page_cache: None,
                        children: BTreeMap::new(),
                        dname: DName::from("/"),
                        vfs_inode_id: generate_inode_id(),
                        parent: self_ref.clone(),
                        self_ref: self_ref.clone(),
                        special_node: None,
                        cached_file_size: None,
                        cached_mtime: None,
                        dirty_state: super::inode::InodeDirtyState::empty(),
                    }),
                    Mutex::new(()),
                    RwSem::new(()),
                    Mutex::new(()),
                    Ext4InodeLifecycle::new(),
                    vfs::InodeRetentionState::new(),
                    SpinLock::new(None),
                    SpinLock::new(false),
                    self_ref.clone(),
                    SpinLock::new(Weak::new()),
                )
            });

        let fs = Arc::new(Ext4FileSystem {
            fs,
            raw_dev,
            root_inode,
            dirty_inodes: Mutex::new(Vec::new()),
            inode_table: Mutex::new(BTreeMap::new()),
            inode_reuse_barrier: RwSem::new(()),
            lifecycle_error: Mutex::new(None),
            eviction_queue: SpinLock::new(Ext4EvictionQueueState::default()),
            eviction_wait: WaitQueue::default(),
            _mount_options: mount_options,
        });

        let mut guard = fs.root_inode.0.lock();
        guard.fs_ptr = Arc::downgrade(&fs);
        drop(guard);
        *fs.root_inode.9.lock() = Arc::downgrade(&fs);
        fs.inode_table.lock().insert(
            another_ext4::EXT4_ROOT_INO,
            CanonicalInodeEntry {
                inode: Arc::downgrade(&fs.root_inode),
                lifecycle: fs.root_inode.lifecycle().clone(),
            },
        );

        Ok(fs)
    }
}

impl Drop for Ext4InodeTombstone {
    fn drop(&mut self) {
        if self.resolved {
            return;
        }
        if let Some(fs) = self.fs.upgrade() {
            let error = SystemError::EIO;
            *fs.lifecycle_error.lock() = Some(error.clone());
            if fs
                .finish_tombstone(
                    self,
                    Ext4InodeLifecycleState::Poisoned(error.clone()),
                    false,
                )
                .is_err()
            {
                // The canonical table may itself be inconsistent. Always wake waiters on
                // the tombstone's original lifecycle before fail-stopping the filesystem.
                self.lifecycle
                    .set_state(Ext4InodeLifecycleState::Poisoned(error));
            }
            log::error!(
                "ext4: unresolved inode tombstone for inode {}, filesystem fail-stopped",
                self.inode_num
            );
        } else {
            self.lifecycle
                .set_state(Ext4InodeLifecycleState::Poisoned(SystemError::EIO));
        }
    }
}

impl MountableFileSystem for Ext4FileSystem {
    fn make_fs(
        data: Option<&dyn FileSystemMakerData>,
    ) -> Result<Arc<dyn FileSystem + 'static>, SystemError> {
        let mount_data = data
            .and_then(|d| d.as_any().downcast_ref::<Ext4MountData>())
            .ok_or(SystemError::EINVAL)?;

        Self::from_gendisk(mount_data.gendisk.clone())
    }
    fn make_mount_data(
        _raw_data: Option<&str>,
        source: &str,
    ) -> Result<Option<Arc<dyn FileSystemMakerData + 'static>>, SystemError> {
        let mount_data = Ext4MountData::from_source(source).map_err(|e| {
            log::error!(
                "Failed to create Ext4 mount data from source '{}': {:?}",
                source,
                e
            );
            e
        })?;
        Ok(Some(Arc::new(mount_data)))
    }
}

register_mountable_fs!(Ext4FileSystem, EXT4FSMAKER, "ext4");

pub struct Ext4MountData {
    gendisk: Arc<GenDisk>,
}

impl FileSystemMakerData for Ext4MountData {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

impl Ext4MountData {
    fn from_source(path: &str) -> Result<Self, SystemError> {
        let pcb = ProcessManager::current_pcb();
        let (current_node, rest_path) = user_path_at(&pcb, AtFlags::AT_FDCWD.bits(), path)?;
        let inode = current_node.lookup_follow_symlink(&rest_path, VFS_MAX_FOLLOW_SYMLINK_TIMES)?;
        if !inode.metadata()?.file_type.eq(&vfs::FileType::BlockDevice) {
            return Err(SystemError::ENOTBLK);
        }

        let disk = inode.dname()?;

        if let Some(gendisk) = try_find_gendisk(disk.0.as_str()) {
            return Ok(Self { gendisk });
        }
        Err(SystemError::ENOENT)
    }
}

impl core::fmt::Debug for Ext4FileSystem {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "ext4")
    }
}
