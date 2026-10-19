use crate::arch::MMArch;
use crate::exception::workqueue::{Work, WorkQueue};
use crate::filesystem::vfs::inode_lifecycle::{EvictionEpoch, InodeRetentionState};
use crate::filesystem::vfs::syscall::RenameFlags;
use crate::libs::{spinlock::SpinLock, wait_queue::WaitQueue};
use crate::mm::MemoryManagementArch;
use alloc::string::ToString;
use alloc::{
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::cmp::Ordering;
use core::intrinsics::unlikely;
use core::num::NonZeroUsize;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};

#[derive(Debug, Default)]
struct FATReclaimQueue {
    next: u64,
    completed: u64,
    sealed: bool,
    error: Option<SystemError>,
}

#[derive(Debug)]
struct FATInodeLifetime {
    retention: InodeRetentionState,
    detached: AtomicBool,
    // release() can run under arbitrary VFS locks: never take the inode mutex.
    owner: SpinLock<(Weak<LockedFATInode>, Weak<FATFileSystem>)>,
}

use core::{any::Any, fmt::Debug};
use hashbrown::HashMap;
use log::error;
use lru::LruCache;
use system_error::SystemError;

use crate::driver::base::block::gendisk::{GenDisk, GenDiskMountGuard};
use crate::driver::base::device::device_number::DeviceNumber;
use crate::filesystem::page_cache::{AsyncPageCacheBackend, PageCache, PageCacheWritebackDomain};
use crate::filesystem::vfs::mount::filesystem_is_synchronous;
use crate::filesystem::vfs::utils::DName;
use crate::filesystem::vfs::{Magic, SpecialNodeData, SuperBlock};
use crate::ipc::pipe::LockedPipeInode;
use crate::libs::rwsem::RwSem;
use crate::mm::allocation::MemoryAllocationError;
use crate::mm::fault::{PageFaultHandler, PageFaultMessage};
use crate::mm::VmFaultReason;
use crate::{
    driver::base::block::{block_device::LBA_SIZE, disk_info::Partition, SeekFrom},
    filesystem::vfs::{
        file::{FileFlags, FilePrivateData},
        vcore::generate_inode_id,
        FileSystem, FileType, IndexNode, InodeFlags, InodeId, InodeMode, LinkMutationCoordinator,
        LinkRemovalOutcome, Metadata, RenameOutcome, SetMetadataMask,
    },
    libs::{
        mutex::{Mutex, MutexGuard},
        vec_cursor::VecCursor,
    },
    time::PosixTimeSpec,
};

use super::entry::FATFile;
use super::utils::{to_search_name, to_search_name_string};
use super::{
    bpb::{BiosParameterBlock, FATType},
    entry::{FATDir, FATDirEntry, FATDirIter, FATEntry},
    utils::RESERVED_CLUSTERS,
};

const FAT_MAX_NAMELEN: u64 = 255;
const FAT_NEGATIVE_CHILDREN_CACHE_SIZE: usize = 256;

impl Default for FATInodeLifetime {
    fn default() -> Self {
        Self {
            retention: InodeRetentionState::new(),
            detached: AtomicBool::new(false),
            owner: SpinLock::new((Weak::new(), Weak::new())),
        }
    }
}

/// FAT32文件系统的最大的文件大小
pub const MAX_FILE_SIZE: u64 = 0xffff_ffff;

/// 每次清零写入时的缓冲区上限（避免分配过大内存）
pub const ZERO_BUF_SIZE: usize = 512 * 1024; // 512KB

/// @brief 表示当前簇和上一个簇的关系的结构体
/// 定义这样一个结构体的原因是，FAT文件系统的文件中，前后两个簇具有关联关系。
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Cluster {
    pub cluster_num: u64,
    pub parent_cluster: u64,
}

pub type ClusterID = u64;

impl PartialOrd for Cluster {
    /// @brief 根据当前簇号比较大小
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        return self.cluster_num.partial_cmp(&other.cluster_num);
    }
}

impl PartialEq for Cluster {
    /// @brief 根据当前簇号比较是否相等
    fn eq(&self, other: &Self) -> bool {
        self.cluster_num == other.cluster_num
    }
}

impl Eq for Cluster {}

#[derive(Debug)]
pub struct FATFileSystem {
    writeback_domain: Arc<PageCacheWritebackDomain>,
    reclaim_queue: SpinLock<FATReclaimQueue>,
    /// Serial within this filesystem, independent of other devices.
    reclaim_worker: Arc<WorkQueue>,
    reclaim_wait: WaitQueue,
    /// 当前文件系统所在的分区
    pub(super) storage: super::io::FatStorage,
    /// Keeps the device bound until final shutdown completes. The lock also
    /// serializes shutdown with the fallback performed by Drop.
    device_mount_holder: Mutex<Option<GenDiskMountGuard>>,
    /// 当前文件系统的BOPB
    pub bpb: BiosParameterBlock,
    /// 当前文件系统的第一个数据扇区（相对分区开始位置）
    pub first_data_sector: u64,
    /// 文件系统信息结构体
    pub fs_info: Arc<LockedFATFsInfo>,
    /// 文件系统的根inode
    root_inode: Arc<LockedFATInode>,
    /// Bounded, preallocated FAT-sector write-through cache and table I/O lock.
    pub(super) fat_cache: Mutex<super::table::FatSectorCache>,
    /// Serialize FAT chain allocation and release.
    ///
    /// Selecting a free cluster and publishing it in the FAT must be one
    /// filesystem-wide transaction.  Per-inode locking is insufficient
    /// because writeback for different files can run concurrently and select
    /// the same free entry.  This mirrors Linux's `msdos_sb_info::fat_lock`.
    pub(super) fat_lock: Mutex<()>,
    /// Invalidates allocation statistics when any FAT chain edge changes.
    /// Saturation disables memoization rather than allowing stamp reuse.
    pub(super) fat_chain_generation: AtomicU64,
    /// 目录项扇区读-改-写串行化锁。
    ///
    /// FAT 的 `ShortDirEntry`/`LongDirEntry` 只占 32 字节，而底层 gendisk 的写入粒度是
    /// 一个 LBA 扇区（典型 512B，可容纳 16 个目录项）。`flush` 的实现是先把整扇区读入
    /// 内存，修改其中 32 字节，再把整扇区写回磁盘。
    ///
    /// 在多线程 / 多 CPU 环境下，若下列两条路径并发触发：
    /// - 主线程在父目录中新建文件（写入新的 long/short 目录项到扇区 S）
    /// - 另一文件的 pagecache 异步回写 worker 触发 `ensure_len`，更新该文件短目录项
    ///   (同样是扇区 S 的 RMW)
    ///
    /// 二者的 RMW 窗口若交错，后写入者会用自己先前读到的"旧扇区 + 自己的 32 字节"
    /// 覆盖对方刚写入的新目录项，导致新文件的目录项在磁盘上被静默抹掉，后续 `find`
    /// 返回 `ENOENT`。该问题在慢速 TCG 仿真下极易复现，在 KVM 下时序紧凑难以触发。
    ///
    /// 本锁用于把所有目录项扇区的 RMW 串行化，消除上述竞争。所有持锁位置都已确认
    /// 不会同时持有 inode 锁的反向依赖，不存在死锁风险。
    dirent_io_lock: Mutex<()>,
}

/// FAT文件系统的Inode
#[derive(Debug)]
pub struct LockedFATInode(
    Mutex<FATInode>,
    RwSem<()>,
    LinkMutationCoordinator,
    FATInodeLifetime,
);

#[derive(Debug)]
pub struct LockedFATFsInfo(Mutex<FATFsInfo>);

pub(super) enum AttachReservedError {
    Isolated(SystemError),
    Ambiguous(SystemError),
}

impl LockedFATFsInfo {
    #[inline]
    pub fn new(fs_info: FATFsInfo) -> Self {
        return Self(Mutex::new(fs_info));
    }
}

pub struct FATInode {
    /// 指向父Inode的弱引用
    parent: Weak<LockedFATInode>,
    /// 指向自身的弱引用
    self_ref: Weak<LockedFATInode>,
    /// 子Inode的map. 该数据结构用作缓存区。其中，它的key表示inode的名称。
    /// 请注意，由于FAT的查询过程对大小写不敏感，因此我们选择让key全部是大写的，方便统一操作。
    children: HashMap<String, Arc<LockedFATInode>>,
    /// 有界负向目录项缓存。FAT 没有全局 VFS dcache；动态链接器等热路径会反复探测
    /// 不存在的 hwcaps/tls 路径。缓存 ENOENT 可以避免每次 miss 都重新线性扫描目录。
    negative_children: LruCache<String, ()>,
    /// 当前inode的元数据
    metadata: Metadata,
    /// Successful allocation scan: (chain generation, first cluster, sectors).
    allocated_blocks_cache: Option<(u64, ClusterID, usize)>,
    /// 指向inode所在的文件系统对象的指针
    fs: Weak<FATFileSystem>,

    /// 根据不同的Inode类型，创建不同的私有字段
    inode_type: FATDirEntry,

    /// 若该节点是特殊文件节点，该字段则为真正的文件节点
    special_node: Option<SpecialNodeData>,

    /// 目录名
    dname: DName,

    /// 页缓存
    page_cache: Option<Arc<PageCache>>,
}

impl Debug for FATInode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FATInode")
            .field("inode_id", &self.metadata.inode_id)
            .finish()
    }
}

impl FATInode {
    fn metadata_with_allocated_blocks(&mut self) -> Result<Metadata, SystemError> {
        let mut metadata = self.metadata.clone();
        if !matches!(metadata.file_type, FileType::File | FileType::Dir) {
            metadata.blocks = 0;
            return Ok(metadata);
        }
        let fs = self.fs.upgrade().ok_or(SystemError::EIO)?;
        metadata.blocks = match &mut self.inode_type {
            FATDirEntry::File(file) | FATDirEntry::VolId(file) => {
                // Cached logical EOF may grow before buffered writeback has
                // allocated a chain. Only a nonempty disk dirent with no
                // first cluster proves corruption.
                if file.first_cluster.cluster_num == 0 && file.size() != 0 {
                    return Err(SystemError::EIO);
                }
                file.allocated_sectors(&fs)?
            }
            FATDirEntry::Dir(dir) if dir.root_offset.is_some() => {
                let bytes = (fs.bpb.root_entries_cnt as u64)
                    .checked_mul(32)
                    .ok_or(SystemError::EOVERFLOW)?;
                // Linux fat_read_root reports its fixed root area rounded to
                // cluster size, even though FAT12/16 has no root chain.
                let cluster_bytes = fs.bytes_per_cluster();
                let allocated_bytes = bytes
                    .div_ceil(cluster_bytes)
                    .checked_mul(cluster_bytes)
                    .ok_or(SystemError::EOVERFLOW)?;
                usize::try_from(allocated_bytes / 512).map_err(|_| SystemError::EOVERFLOW)?
            }
            FATDirEntry::Dir(dir) => {
                if dir.first_cluster.cluster_num == 0 {
                    return Err(SystemError::EIO);
                }
                let head = dir.first_cluster;
                self.cached_chain_sectors(&fs, head)?
            }
            FATDirEntry::UnInit => return Err(SystemError::EIO),
        };
        Ok(metadata)
    }

    fn cached_chain_sectors(
        &mut self,
        fs: &FATFileSystem,
        head: Cluster,
    ) -> Result<usize, SystemError> {
        if head.cluster_num == 0 {
            return Ok(0);
        }
        let generation = fs.fat_chain_generation.load(AtomicOrdering::Acquire);
        if generation != u64::MAX {
            if let Some((stamp, first, sectors)) = self.allocated_blocks_cache {
                if stamp == generation && first == head.cluster_num {
                    return Ok(sectors);
                }
            }
        }
        let (generation, sectors) = fs.allocated_chain_sectors(head)?;
        self.allocated_blocks_cache =
            (generation != u64::MAX).then_some((generation, head.cluster_num, sectors));
        Ok(sectors)
    }
    /// 将inode的元数据与磁盘同步
    pub fn synchronize_metadata(&mut self) {
        match &self.inode_type {
            FATDirEntry::File(f) | FATDirEntry::VolId(f) => {
                self.metadata.size = f.size() as i64;
            }
            FATDirEntry::Dir(d) => {
                self.metadata.size = d.size(&self.fs.upgrade().unwrap().clone()) as i64;
            }
            FATDirEntry::UnInit => {
                error!("update_metadata: Uninitialized FATDirEntry: {:?}", self);
                return;
            }
        };
    }

    /// Data-page completion changes logical EOF, not the write operation's
    /// timestamp. In particular delayed writeback must not replace utimens.
    pub fn update_metadata(&mut self, size: Option<i64>) {
        if let Some(new_size) = size {
            self.metadata.size = new_size;
        }
    }

    fn is_root(&self) -> bool {
        self.self_ref.ptr_eq(&self.parent)
    }

    /// Commit only time fields from the current slot. Structure already
    /// changed by a truncate/namespace operation is never rolled back here.
    fn apply_metadata(
        &mut self,
        requested: &Metadata,
        mask: SetMetadataMask,
    ) -> Result<(), SystemError> {
        let mut proposed = self.metadata.clone();
        crate::filesystem::vfs::merge_metadata_masked(&mut proposed, requested, mask);
        if self.is_root() {
            proposed.atime = PosixTimeSpec::default();
            proposed.mtime = PosixTimeSpec::default();
            proposed.ctime = PosixTimeSpec::default();
            proposed.btime = PosixTimeSpec::default();
        } else {
            if mask.contains(SetMetadataMask::ATIME) {
                proposed.atime = super::time::truncate_atime(proposed.atime);
            }
            if mask.contains(SetMetadataMask::MTIME) {
                proposed.mtime = super::time::truncate_mtime(proposed.mtime);
                proposed.ctime = proposed.mtime;
            } else {
                // FAT has no independent POSIX change-time field.
                proposed.ctime = self.metadata.ctime;
            }
            // Open-unlinked files and removed directories no longer own the
            // slot, even when their descriptors still keep the inode alive.
            if mask.intersects(SetMetadataMask::ATIME | SetMetadataMask::MTIME)
                && self.metadata.nlinks != 0
                && matches!(self.metadata.file_type, FileType::File | FileType::Dir)
            {
                let fs = self.fs.upgrade().ok_or(SystemError::EIO)?;
                self.inode_type.commit_times(&fs, &proposed)?;
            }
        }
        self.metadata = proposed;
        Ok(())
    }

    fn touch_modified(&mut self) -> Result<(), SystemError> {
        let mut requested = self.metadata.clone();
        requested.mtime = crate::time::timekeeping::realtime_now();
        self.apply_metadata(&requested, SetMetadataMask::MTIME)
    }

    /// vfat_unlink/rmdir update the removed inode after clear_nlink. The old
    /// descriptor sees the deletion time, but it no longer owns any disk slot.
    fn revoke_directory_slot(&mut self) {
        if let FATDirEntry::File(file) | FATDirEntry::VolId(file) = &mut self.inode_type {
            file.detach();
        }
        self.metadata.nlinks = 0;
    }

    fn touch_deleted(&mut self) -> Result<(), SystemError> {
        let mut requested = self.metadata.clone();
        let now = crate::time::timekeeping::realtime_now();
        requested.atime = now;
        requested.mtime = now;
        self.apply_metadata(&requested, SetMetadataMask::ATIME | SetMetadataMask::MTIME)
    }

    fn negative_children_cache() -> LruCache<String, ()> {
        LruCache::new(NonZeroUsize::new(FAT_NEGATIVE_CHILDREN_CACHE_SIZE).unwrap())
    }

    fn invalidate_negative_children(&mut self) {
        self.negative_children = Self::negative_children_cache();
    }

    fn mark_child_negative(&mut self, search_name: String) {
        self.negative_children.put(search_name, ());
    }

    fn mark_child_absent(&mut self, name: &str, canonical_name: &str) {
        let canonical_key = to_search_name(canonical_name);
        self.children.remove(&canonical_key);
        self.invalidate_negative_children();
        self.mark_child_negative(canonical_key);
        self.mark_child_negative(to_search_name(name));
    }

    fn find(&mut self, name: &str) -> Result<Arc<LockedFATInode>, SystemError> {
        match &self.inode_type {
            FATDirEntry::Dir(d) => {
                let search_name = to_search_name(name);
                // 尝试在缓存区查找
                if let Some(entry) = self.children.get(&search_name) {
                    return Ok(entry.clone());
                }
                if self.negative_children.get(&search_name).is_some() {
                    return Err(SystemError::ENOENT);
                }
                // 在缓存区找不到
                // 在磁盘查找
                let fat_entry: FATDirEntry =
                    match d.find_entry(name, None, None, self.fs.upgrade().unwrap()) {
                        Ok(entry) => entry,
                        Err(SystemError::ENOENT) => {
                            self.mark_child_negative(search_name);
                            return Err(SystemError::ENOENT);
                        }
                        Err(e) => return Err(e),
                    };
                // LFN and its short alias select one physical entry and must
                // share its inode, page cache and directory-slot authority.
                let canonical_name = fat_entry.name();
                let canonical_key = to_search_name(&canonical_name);
                if let Some(inode) = self.children.get(&canonical_key) {
                    return Ok(inode.clone());
                }
                let dname = DName::from(canonical_name);
                // 创建新的inode
                let entry_inode: Arc<LockedFATInode> = LockedFATInode::new(
                    dname,
                    self.fs.upgrade().unwrap(),
                    self.self_ref.clone(),
                    fat_entry,
                )?;
                // 加入缓存区, 由于FAT文件系统的大小写不敏感问题，因此存入缓存区的key应当是全大写的
                self.children
                    .insert(canonical_key.clone(), entry_inode.clone());
                self.negative_children.pop(&canonical_key);
                self.negative_children.pop(&search_name);
                return Ok(entry_inode);
            }
            FATDirEntry::UnInit => {
                panic!(
                    "Uninitialized FAT Inode, fs = {:?}, inode={self:?}",
                    self.fs
                )
            }
            _ => {
                return Err(SystemError::ENOTDIR);
            }
        }
    }
}

impl LockedFATInode {
    pub fn new(
        dname: DName,
        fs: Arc<FATFileSystem>,
        parent: Weak<LockedFATInode>,
        inode_type: FATDirEntry,
    ) -> Result<Arc<LockedFATInode>, SystemError> {
        let file_type = if let FATDirEntry::Dir(_) = inode_type {
            FileType::Dir
        } else {
            FileType::File
        };

        let inode: Arc<LockedFATInode> = Arc::new(LockedFATInode(
            Mutex::new(FATInode {
                parent,
                self_ref: Weak::default(),
                children: HashMap::new(),
                negative_children: FATInode::negative_children_cache(),
                fs: Arc::downgrade(&fs),
                inode_type,
                metadata: Metadata {
                    dev_id: 0,
                    inode_id: generate_inode_id(),
                    size: 0,
                    blk_size: fs.bpb.bytes_per_sector as usize,
                    blocks: 0,
                    atime: PosixTimeSpec::default(),
                    mtime: PosixTimeSpec::default(),
                    ctime: PosixTimeSpec::default(),
                    btime: PosixTimeSpec::default(),
                    file_type,
                    mode: InodeMode::S_IRWXUGO,
                    flags: InodeFlags::empty(),
                    nlinks: if file_type == FileType::Dir { 2 } else { 1 },
                    uid: 0,
                    gid: 0,
                    raw_dev: DeviceNumber::default(),
                },
                allocated_blocks_cache: None,
                special_node: None,
                dname,
                page_cache: None,
            }),
            RwSem::new(()),
            LinkMutationCoordinator::new(),
            FATInodeLifetime::default(),
        ));

        if !inode.0.lock().inode_type.is_dir() {
            let backend = Arc::new(AsyncPageCacheBackend::new(
                Arc::downgrade(&inode) as Weak<dyn IndexNode>
            ));
            let page_cache = PageCache::new_file(
                Arc::downgrade(&inode) as Weak<dyn IndexNode>,
                backend,
                &fs.writeback_domain,
            )?;
            inode.0.lock().page_cache = Some(page_cache);
        }

        inode.0.lock().self_ref = Arc::downgrade(&inode);
        *inode.3.owner.lock() = (Arc::downgrade(&inode), Arc::downgrade(&fs));

        {
            let mut guard = inode.0.lock();
            guard.synchronize_metadata();
            if let Some(entry) = guard.inode_type.short_dir_entry() {
                let (atime, mtime, btime) = entry.times();
                guard.metadata.atime = atime;
                guard.metadata.mtime = mtime;
                guard.metadata.ctime = mtime;
                guard.metadata.btime = btime;
            }
        }

        Ok(inode)
    }

    #[inline(never)]
    fn rename_file_in_current_dir(
        &self,
        old_name: &str,
        new_name: &str,
        flags: RenameFlags,
    ) -> Result<RenameOutcome, SystemError> {
        if flags.contains(RenameFlags::WHITEOUT) {
            return Err(SystemError::EINVAL);
        }
        if old_name == new_name {
            if flags.contains(RenameFlags::NOREPLACE) {
                return Err(SystemError::EEXIST);
            }
            return Ok(RenameOutcome::NoOp);
        }
        let old_key = to_search_name(old_name);
        let new_key = to_search_name(new_name);
        let mut guard = self.0.lock();
        if old_key == new_key {
            guard.find(old_name)?;
            if flags.contains(RenameFlags::NOREPLACE) {
                return Err(SystemError::EEXIST);
            }
            return Ok(RenameOutcome::NoOp);
        }
        let old_inode = guard.find(old_name)?;
        let new_inode = match guard.find(new_name) {
            Ok(inode) => Some(inode),
            Err(SystemError::ENOENT) => None,
            Err(error) => return Err(error),
        };
        let had_new = new_inode.is_some();
        if flags.contains(RenameFlags::NOREPLACE) && new_inode.is_some() {
            return Err(SystemError::EEXIST);
        }

        if flags.contains(RenameFlags::EXCHANGE) {
            if new_inode.is_none() {
                return Err(SystemError::ENOENT);
            }
            // TODO: Implement EXCHANGE logic
            return Err(SystemError::EINVAL);
        }

        if new_inode
            .as_ref()
            .is_some_and(|inode| Arc::ptr_eq(inode, &old_inode))
        {
            return Ok(RenameOutcome::NoOp);
        }

        // 对目标inode上锁，以防更改
        let mut old_inode_guard = old_inode.0.lock();
        let fs = old_inode_guard.fs.upgrade().unwrap();
        let old_dir = match &guard.inode_type {
            FATDirEntry::File(_) | FATDirEntry::VolId(_) => {
                return Err(SystemError::ENOTDIR);
            }
            FATDirEntry::Dir(d) => d,
            FATDirEntry::UnInit => {
                error!("FATFS: param: Inode_type uninitialized.");
                return Err(SystemError::EROFS);
            }
        };
        // Keep both inode owners stable until the complete disk protocol has
        // committed. In particular the victim cannot enter reclamation early.
        let mut victim_guard = new_inode.as_ref().map(|inode| inode.0.lock());
        let old_key = to_search_name(&old_inode_guard.dname.0);
        let new_dname = victim_guard
            .as_ref()
            .map_or_else(|| DName::from(new_name), |inode| inode.dname.clone());
        let new_key = to_search_name(&new_dname.0);
        let entry = old_dir.rename_into(fs.clone(), old_dir, old_name, new_name, false)?;
        old_inode_guard.inode_type = entry;
        old_inode_guard.dname = new_dname;
        if let Some(victim) = &mut victim_guard {
            victim.revoke_directory_slot();
            new_inode
                .as_ref()
                .unwrap()
                .3
                .detached
                .store(true, AtomicOrdering::Release);
        }
        drop(victim_guard);
        if let Some(victim) = &new_inode {
            victim.schedule_reclaim();
        }
        let old_inode = guard.children.remove(&old_key).unwrap();
        guard.invalidate_negative_children();
        guard.mark_child_negative(old_key);
        guard.children.insert(new_key, old_inode);
        if had_new && old_inode_guard.metadata.file_type == FileType::Dir {
            guard.metadata.nlinks = guard.metadata.nlinks.saturating_sub(1);
        }
        // The namespace has committed. Metadata writeback errors must be
        // reported through errseq, not turn success into a stale VFS rename.
        if let Err(error) = guard.touch_modified() {
            fs.writeback_domain.record_writeback_error(error);
        }

        Ok(RenameOutcome::Moved {
            replaced: had_new.then_some(LinkRemovalOutcome::LastLink),
        })
    }

    #[inline(never)]
    fn move_to_another_dir(
        &self,
        old_name: &str,
        new_name: &str,
        target: &Arc<dyn IndexNode>,
        flags: RenameFlags,
    ) -> Result<RenameOutcome, SystemError> {
        if flags.contains(RenameFlags::WHITEOUT) {
            return Err(SystemError::EINVAL);
        }
        let mut old_guard = self.0.lock();
        let other: &LockedFATInode = target
            .downcast_ref::<LockedFATInode>()
            .ok_or(SystemError::EPERM)?;

        let mut new_guard = other.0.lock();
        let old_inode: Arc<LockedFATInode> = old_guard.find(old_name)?;
        let new_inode = match new_guard.find(new_name) {
            Ok(inode) => Some(inode),
            Err(SystemError::ENOENT) => None,
            Err(error) => return Err(error),
        };

        if flags.contains(RenameFlags::NOREPLACE) && new_inode.is_some() {
            return Err(SystemError::EEXIST);
        }

        if flags.contains(RenameFlags::EXCHANGE) {
            if new_inode.is_none() {
                return Err(SystemError::ENOENT);
            }
            // TODO: Implement EXCHANGE logic
            return Err(SystemError::EINVAL);
        }

        // 对目标inode上锁，以防更改
        let mut old_inode_guard: MutexGuard<FATInode> = old_inode.0.lock();
        // let new_inode_guard = new_inode.0.lock();
        let fs = old_inode_guard.fs.upgrade().unwrap();
        let old_dir = match &old_guard.inode_type {
            FATDirEntry::File(_) | FATDirEntry::VolId(_) => {
                return Err(SystemError::ENOTDIR);
            }
            FATDirEntry::Dir(d) => d,
            FATDirEntry::UnInit => {
                error!("FATFS: param: Inode_type uninitialized.");
                return Err(SystemError::EROFS);
            }
        };
        let new_dir = match &new_guard.inode_type {
            FATDirEntry::File(_) | FATDirEntry::VolId(_) => {
                return Err(SystemError::ENOTDIR);
            }
            FATDirEntry::Dir(d) => d,
            FATDirEntry::UnInit => {
                error!("FATFA: param: Inode_type uninitialized.");
                return Err(SystemError::EROFS);
            }
        };

        // A target equal to the already locked old parent contains the
        // source, hence is nonempty. Do not recursively lock its mutex.
        if new_inode
            .as_ref()
            .is_some_and(|inode| core::ptr::eq(self, Arc::as_ptr(inode)))
        {
            return Err(if old_inode_guard.metadata.file_type == FileType::Dir {
                SystemError::ENOTEMPTY
            } else {
                SystemError::EISDIR
            });
        }
        let mut victim_guard = new_inode.as_ref().map(|inode| inode.0.lock());
        let old_key = to_search_name(&old_inode_guard.dname.0);
        let new_dname = victim_guard
            .as_ref()
            .map_or_else(|| DName::from(new_name), |inode| inode.dname.clone());
        let new_key = to_search_name(&new_dname.0);
        let entry = old_dir.rename_into(fs.clone(), new_dir, old_name, new_name, true)?;
        old_inode_guard.inode_type = entry;
        old_inode_guard.parent = new_guard.self_ref.clone();
        old_inode_guard.dname = new_dname;
        if old_inode_guard.metadata.file_type == FileType::Dir {
            old_guard.metadata.nlinks = old_guard.metadata.nlinks.saturating_sub(1);
            if new_inode.is_none() {
                new_guard.metadata.nlinks += 1;
            }
        }
        if let Some(victim) = &mut victim_guard {
            victim.revoke_directory_slot();
            new_inode
                .as_ref()
                .unwrap()
                .3
                .detached
                .store(true, AtomicOrdering::Release);
        }
        drop(victim_guard);
        if let Some(victim) = &new_inode {
            victim.schedule_reclaim();
        }
        let old_inode = old_guard.children.remove(&old_key).unwrap();
        old_guard.invalidate_negative_children();
        old_guard.mark_child_negative(old_key);
        new_guard.invalidate_negative_children();
        new_guard.children.insert(new_key, old_inode);
        for result in [old_guard.touch_modified(), new_guard.touch_modified()] {
            if let Err(error) = result {
                fs.writeback_domain.record_writeback_error(error);
            }
        }

        Ok(RenameOutcome::Moved {
            replaced: new_inode.map(|_| LinkRemovalOutcome::LastLink),
        })
    }
}

/// FsInfo结构体（内存中的一份拷贝，当卸载卷或者sync的时候，把它写入磁盘）
#[derive(Debug)]
pub struct FATFsInfo {
    /// Lead Signature - must equal 0x41615252
    lead_sig: u32,
    /// Value must equal 0x61417272
    struc_sig: u32,
    /// 空闲簇数目
    free_count: u32,
    /// 第一个空闲簇的位置（不一定准确，仅供加速查找）
    next_free: u32,
    /// 0xAA550000
    trail_sig: u32,
    /// Dirty flag to flush to disk
    dirty: bool,
    /// FSInfo byte offset relative to this GenDisk (not the whole device).
    /// Not present for FAT12 and FAT16
    offset: Option<u64>,
}

impl LockedFATInode {
    pub(super) fn remove_directory_name(
        &self,
        parent: &FATDir,
        fs: Arc<FATFileSystem>,
        name: &str,
    ) -> Result<(), SystemError> {
        let result = {
            let mut inode = self.0.lock();
            if !matches!(inode.inode_type, FATDirEntry::Dir(_)) {
                return Err(SystemError::ENOTDIR);
            }
            parent
                .remove_entry(fs, name, true, || {
                    inode.revoke_directory_slot();
                    self.3.detached.store(true, AtomicOrdering::Release);
                })
                .map(|_| ())
        };
        // Like fat_evict_inode(), keep the chain allocated while an old
        // directory descriptor owns retention. This is the same final-free
        // queue as files, not a second directory lifetime or a stat snapshot.
        self.schedule_reclaim();
        result
    }

    /// The caller serializes directory mutation. Keep the victim inode locked
    /// through slot removal and detachment so writeback cannot touch a reused slot.
    pub(super) fn remove_file_name(
        &self,
        parent: &FATDir,
        fs: Arc<FATFileSystem>,
        name: &str,
    ) -> Result<(), SystemError> {
        let result = {
            let mut inode = self.0.lock();
            if !matches!(
                inode.inode_type,
                FATDirEntry::File(_) | FATDirEntry::VolId(_)
            ) {
                return Err(SystemError::EINVAL);
            }
            parent.remove(fs, name, false, || {
                inode.revoke_directory_slot();
                self.3.detached.store(true, AtomicOrdering::Release);
            })
        };
        // Only a known unlink/replacement may arm final chain reclamation.
        // A transient source rename handoff deliberately never sets this flag.
        self.schedule_reclaim();
        result
    }

    fn schedule_reclaim(&self) {
        if !self.3.detached.load(AtomicOrdering::Acquire)
            || self.3.retention.try_begin_freeing().is_err()
        {
            return;
        }
        let (inode, fs) = {
            let owner = self.3.owner.lock();
            (owner.0.upgrade(), owner.1.upgrade())
        };
        let (Some(inode), Some(fs)) = (inode, fs) else {
            return;
        };
        // FIFO publication and epoch assignment share the queue lock. Work
        // owns both objects; neither release nor Drop performs filesystem I/O.
        let mut queue = fs.reclaim_queue.lock();
        if queue.sealed {
            fs.writeback_domain
                .record_writeback_error(SystemError::EBUSY);
            queue.error.get_or_insert(SystemError::EBUSY);
            return;
        }
        let Some(epoch) = queue.next.checked_add(1) else {
            fs.writeback_domain
                .record_writeback_error(SystemError::EOVERFLOW);
            queue.error.get_or_insert(SystemError::EOVERFLOW);
            return;
        };
        queue.next = epoch;
        let worker_fs = fs.clone();
        fs.reclaim_worker.enqueue(Work::new(move || {
            let result = inode.reclaim_detached(&worker_fs);
            let mut queue = worker_fs.reclaim_queue.lock();
            if let Err(error) = result {
                // Replaying a partially completed free could double-free a
                // reused cluster. Preserve the error, never retry the old head.
                worker_fs
                    .writeback_domain
                    .record_writeback_error(error.clone());
                queue.error.get_or_insert(error);
            }
            queue.completed = epoch;
            drop(queue);
            worker_fs.reclaim_wait.wake_all();
        }));
    }

    fn reclaim_detached(&self, fs: &Arc<FATFileSystem>) -> Result<(), SystemError> {
        let (cache, first) = {
            let inode = self.0.lock();
            (inode.page_cache.clone(), inode.inode_type.first_cluster())
        };
        if let Some(cache) = cache {
            cache.manager().resize(0)?;
        }
        if first.cluster_num >= RESERVED_CLUSTERS as u64 {
            fs.deallocate_cluster_chain(first)?;
        }
        Ok(())
    }
}

impl FileSystem for FATFileSystem {
    fn prepare_write(
        &self,
    ) -> Result<Vec<crate::filesystem::vfs::mount::writer::MountWriteGuard>, SystemError> {
        self.storage.check_writable()?;
        Ok(Vec::new())
    }

    fn on_umount(&self) {
        if let Err(error) = self.shutdown() {
            error!("Umount FAT filesystem failed: errno={error:?}, FS detail:{self:?}");
        }
    }

    fn seal_eviction_queue(&self) -> EvictionEpoch {
        let mut queue = self.reclaim_queue.lock();
        queue.sealed = true;
        EvictionEpoch::new(queue.next)
    }

    fn drain_evictions_through(&self, epoch: EvictionEpoch) -> Result<(), SystemError> {
        self.reclaim_wait
            .wait_until(|| {
                let queue = self.reclaim_queue.lock();
                (queue.completed >= epoch.value()).then(|| queue.error.clone())
            })
            .map_or(Ok(()), Err)
    }

    fn sync_fs(&self, wait: bool) -> Result<(), SystemError> {
        if wait {
            let epoch = EvictionEpoch::new(self.reclaim_queue.lock().next);
            let reclaim_result = self.drain_evictions_through(epoch);
            // Commit the hints under their own lock, then release it before
            // the device barrier. Failed writes remain dirty for a retry.
            let info_result = self.fs_info.0.lock().flush(&self.storage);
            // A sticky reclamation error must not suppress durability for
            // other files whose writeback has already completed.
            let sync_result = self.storage.sync();
            reclaim_result.and(info_result).and(sync_result)?;
        }
        Ok(())
    }

    fn supports_reliable_flush(&self) -> bool {
        self.storage.supports_reliable_flush()
    }

    fn page_cache_writeback_domain(&self) -> Option<&Arc<PageCacheWritebackDomain>> {
        Some(&self.writeback_domain)
    }
    fn root_inode(&self) -> Arc<dyn crate::filesystem::vfs::IndexNode> {
        return self.root_inode.clone();
    }

    fn info(&self) -> crate::filesystem::vfs::FsInfo {
        crate::filesystem::vfs::FsInfo {
            blk_dev_id: self.storage.device_num().data() as usize,
            max_name_len: FAT_MAX_NAMELEN as usize,
        }
    }

    /// @brief 本函数用于实现动态转换。
    /// 具体的文件系统在实现本函数时，最简单的方式就是：直接返回self
    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        "fat"
    }

    fn super_block(&self) -> SuperBlock {
        let mut sb = SuperBlock::new(
            Magic::FAT_MAGIC,
            self.bpb.bytes_per_sector.into(),
            FAT_MAX_NAMELEN,
        );

        // statfs/statvfs 依赖这些字段：
        // - f_frsize/f_bsize: 基本块大小
        // - f_blocks: 总块数（BusyBox df 默认会过滤 f_blocks==0 的挂载项）
        // - f_bfree/f_bavail: 空闲块数
        let frsize = self.bpb.bytes_per_sector as u64;
        sb.frsize = frsize;
        sb.blocks = self.data_sectors();

        if sb.blocks > 0 {
            let free_clusters = self.free_clusters_cached().unwrap_or(0);
            let free_sectors = free_clusters.saturating_mul(self.bpb.sector_per_cluster as u64);
            let free_sectors = core::cmp::min(free_sectors, sb.blocks);
            sb.bfree = free_sectors;
            sb.bavail = free_sectors;
        }

        sb
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
}

impl FATFileSystem {
    /// 探测 gendisk 是否包含 FAT 文件系统
    pub fn probe(gendisk: &Arc<GenDisk>) -> bool {
        BiosParameterBlock::new(gendisk).is_ok()
    }

    /// FAT12允许的最大簇号
    pub const FAT12_MAX_CLUSTER: u32 = 0xFF5;
    /// FAT16允许的最大簇号
    pub const FAT16_MAX_CLUSTER: u32 = 0xFFF5;
    /// FAT32允许的最大簇号
    pub const FAT32_MAX_CLUSTER: u32 = 0x0FFFFFF7;

    /// 计算 FAT 数据区的总扇区数（用于 statfs 的 f_blocks，单位为扇区）。
    #[inline]
    fn data_sectors(&self) -> u64 {
        let total_sectors = if self.bpb.total_sectors_16 != 0 {
            self.bpb.total_sectors_16 as u64
        } else {
            self.bpb.total_sectors_32 as u64
        };
        total_sectors.saturating_sub(self.first_data_sector)
    }

    /// 计算空闲簇数量：优先使用 FSInfo；否则扫描 FAT 表并回填内存缓存。
    fn free_clusters_cached(&self) -> Result<u64, SystemError> {
        let max_cluster = self.max_cluster_number();
        // 先尝试使用 FSInfo（若可用）
        {
            let guard = self.fs_info.0.lock();
            if let Some(n) = guard.count_free_cluster(max_cluster) {
                return Ok(n);
            }
        }

        // FSInfo is unavailable: re-check, scan, and backfill under the same FAT lock the allocator uses.
        // Otherwise a concurrent allocation could occur between the scan and the absolute-value
        // backfill, causing free clusters to be overestimated.
        let _fat_guard = self.fat_lock.lock();
        {
            let guard = self.fs_info.0.lock();
            if let Some(n) = guard.count_free_cluster(max_cluster) {
                return Ok(n);
            }
        }
        let free_clusters = self.scan_free_clusters(max_cluster)?;

        if free_clusters <= u32::MAX as u64 {
            self.fs_info
                .0
                .lock()
                .update_free_count_abs(free_clusters as u32);
        }
        Ok(free_clusters)
    }

    /// 扫描 FAT 表统计空闲簇数量。
    ///
    /// 注意：这是 O(N) 操作，但仅在 FSInfo 不可用时触发，并会写入缓存。
    fn scan_free_clusters(&self, max_cluster: Cluster) -> Result<u64, SystemError> {
        let start_cluster = RESERVED_CLUSTERS as u64;
        let end_cluster = max_cluster.cluster_num;
        if end_cluster < start_cluster {
            return Ok(0);
        }

        match self.bpb.fat_type {
            FATType::FAT32(_) => self.scan_free_clusters_fixed_width(end_cluster, 4, 0x0FFF_FFFF),
            FATType::FAT16(_) => self.scan_free_clusters_fixed_width(end_cluster, 2, 0xFFFF),
            FATType::FAT12(_) => {
                // FAT12 解析较复杂（12-bit packing），这里退化为逐簇读取；FAT12 规模通常较小。
                let mut free = 0u64;
                for c in start_cluster..=end_cluster {
                    if matches!(self.get_fat_entry(Cluster::new(c))?, FATEntry::Unused) {
                        free += 1;
                    }
                }
                Ok(free)
            }
        }
    }

    /// 固定宽度 FAT 表扫描（FAT16=2 bytes, FAT32=4 bytes）。
    ///
    /// `mask` 用于 FAT32 的高 4bit 保留位屏蔽。
    fn scan_free_clusters_fixed_width(
        &self,
        end_cluster: u64,
        entry_bytes: usize,
        mask: u32,
    ) -> Result<u64, SystemError> {
        let start_cluster = RESERVED_CLUSTERS as u64;

        let fat_start_sector = self.fat_start_sector();
        let fat_bytes_offset = fat_start_sector * (self.bpb.bytes_per_sector as u64);

        // 只扫描 [start_cluster, end_cluster] 的 entry。
        let start_byte = start_cluster * entry_bytes as u64;
        let end_byte_exclusive = (end_cluster + 1) * entry_bytes as u64;

        const CHUNK_BYTES: usize = 512 * 1024; // 512KB

        let mut free = 0u64;
        let mut cur = start_byte;
        let mut remaining = end_byte_exclusive.saturating_sub(start_byte);

        while remaining > 0 {
            let mut to_read = core::cmp::min(remaining as usize, CHUNK_BYTES);
            // 对齐到 entry_bytes，避免跨 chunk 解析 entry
            to_read -= to_read % entry_bytes;
            if to_read == 0 {
                to_read = entry_bytes;
            }

            let mut buf = vec![0u8; to_read];
            self.storage
                .read_at_bytes(&mut buf, (fat_bytes_offset + cur) as usize)?;

            for i in (0..buf.len()).step_by(entry_bytes) {
                let val = match entry_bytes {
                    2 => u16::from_le_bytes([buf[i], buf[i + 1]]) as u32,
                    4 => u32::from_le_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]) & mask,
                    _ => 0,
                };
                if val == 0 {
                    free += 1;
                }
            }

            cur += to_read as u64;
            remaining = remaining.saturating_sub(to_read as u64);
        }

        Ok(free)
    }

    pub fn new(gendisk: Arc<GenDisk>) -> Result<Arc<FATFileSystem>, SystemError> {
        let device_mount_holder = gendisk.acquire_mount_holder()?;
        let bpb = BiosParameterBlock::new(&gendisk)?;
        Self::validate_table_geometry(&bpb)?;
        // 从磁盘上读取FAT32文件系统的FsInfo结构体
        let fs_info: FATFsInfo = match bpb.fat_type {
            FATType::FAT32(bpb32) => {
                let fs_info_in_gendisk_bytes_offset =
                    bpb32.fs_info as usize * bpb.bytes_per_sector as usize;
                FATFsInfo::new(
                    &gendisk,
                    fs_info_in_gendisk_bytes_offset,
                    bpb.bytes_per_sector as usize,
                )?
            }
            _ => FATFsInfo::default(),
        };

        // 根目录项占用的扇区数（向上取整）
        let root_dir_sectors: u64 =
            (bpb.root_entries_cnt as u64 * 32).div_ceil(bpb.bytes_per_sector as u64);

        // FAT表大小（单位：扇区）
        let fat_size = if bpb.fat_size_16 != 0 {
            bpb.fat_size_16 as u64
        } else {
            match bpb.fat_type {
                FATType::FAT32(x) => x.fat_size_32 as u64,
                _ => {
                    error!("FAT12 and FAT16 volumes should have non-zero BPB_FATSz16");
                    return Err(SystemError::EINVAL);
                }
            }
        };

        let first_data_sector =
            bpb.rsvd_sec_cnt as u64 + (bpb.num_fats as u64 * fat_size) + root_dir_sectors;

        // 创建文件系统的根节点
        let root_inode: Arc<LockedFATInode> = Arc::new(LockedFATInode(
            Mutex::new(FATInode {
                parent: Weak::default(),
                self_ref: Weak::default(),
                children: HashMap::new(),
                negative_children: FATInode::negative_children_cache(),
                fs: Weak::default(),
                inode_type: FATDirEntry::UnInit,
                metadata: Metadata {
                    dev_id: 0,
                    inode_id: generate_inode_id(),
                    size: 0,
                    blk_size: bpb.bytes_per_sector as usize,
                    blocks: 0,
                    atime: PosixTimeSpec::default(),
                    mtime: PosixTimeSpec::default(),
                    ctime: PosixTimeSpec::default(),
                    btime: PosixTimeSpec::default(),
                    file_type: FileType::Dir,
                    mode: InodeMode::S_IRWXUGO,
                    flags: InodeFlags::empty(),
                    nlinks: 2,
                    uid: 0,
                    gid: 0,
                    raw_dev: DeviceNumber::default(),
                },
                allocated_blocks_cache: None,
                special_node: None,
                dname: DName::default(),
                page_cache: None,
            }),
            RwSem::new(()),
            LinkMutationCoordinator::new(),
            FATInodeLifetime::default(),
        ));

        let result: Arc<FATFileSystem> = Arc::new(FATFileSystem {
            writeback_domain: PageCacheWritebackDomain::new(),
            reclaim_queue: SpinLock::new(FATReclaimQueue::default()),
            reclaim_worker: WorkQueue::try_new_owned("fat_evict")?,
            reclaim_wait: WaitQueue::default(),
            storage: super::io::FatStorage::new(gendisk),
            device_mount_holder: Mutex::new(Some(device_mount_holder)),
            bpb,
            first_data_sector,
            fs_info: Arc::new(LockedFATFsInfo::new(fs_info)),
            root_inode,
            fat_cache: Mutex::new(super::table::FatSectorCache::new()?),
            fat_lock: Mutex::new(()),
            fat_chain_generation: AtomicU64::new(0),
            dirent_io_lock: Mutex::new(()),
        });

        // 对root inode加锁，并继续完成初始化工作
        let mut root_guard: MutexGuard<FATInode> = result.root_inode.0.lock();
        root_guard.inode_type = FATDirEntry::Dir(result.root_dir());
        root_guard.parent = Arc::downgrade(&result.root_inode);
        root_guard.self_ref = Arc::downgrade(&result.root_inode);
        root_guard.fs = Arc::downgrade(&result);
        *result.root_inode.3.owner.lock() =
            (Arc::downgrade(&result.root_inode), Arc::downgrade(&result));
        // 释放锁
        drop(root_guard);

        return Ok(result);
    }

    /// @brief 计算每个簇有多少个字节
    #[inline]
    pub fn bytes_per_cluster(&self) -> u64 {
        return (self.bpb.bytes_per_sector as u64) * (self.bpb.sector_per_cluster as u64);
    }

    /// Undo could not recover the namespace. Quarantine this filesystem's
    /// writes before publishing the same hard-error state to its canonical SB.
    pub(super) fn abort_metadata_io(&self, error: SystemError) {
        self.storage.fail();
        self.writeback_domain.record_writeback_error(error);
        if let Some(sb) = self.writeback_domain.bound_superblock() {
            sb.force_readonly();
        }
    }

    /// 锁定目录项扇区 RMW 串行化互斥量。
    ///
    /// 详见 `FATFileSystem::dirent_io_lock` 字段的注释。所有会触发
    /// `ShortDirEntry::flush` / `LongDirEntry::flush` 的代码路径都必须在执行
    /// "read sector -> modify 32B -> write sector" 前先持有该锁，直到 `write_at` 完成。
    #[inline]
    pub(super) fn lock_dirent_io(&self) -> MutexGuard<'_, ()> {
        self.dirent_io_lock.lock()
    }

    /// @brief 获取当前文件系统的root inode，在分区内的字节偏移量
    pub fn root_dir_bytes_offset(&self) -> u64 {
        match self.bpb.fat_type {
            FATType::FAT32(s) => {
                let first_sec_cluster: u64 = (s.root_cluster as u64 - 2)
                    * (self.bpb.sector_per_cluster as u64)
                    + self.first_data_sector;
                return (self.gendisk_lba_from_offset(first_sec_cluster) * LBA_SIZE) as u64;
            }
            _ => {
                let root_sec = (self.bpb.rsvd_sec_cnt as u64)
                    + (self.bpb.num_fats as u64) * (self.bpb.fat_size_16 as u64);
                return (self.gendisk_lba_from_offset(root_sec) * LBA_SIZE) as u64;
            }
        }
    }

    /// @brief 获取当前文件系统的根目录项区域的结束位置，在分区内的字节偏移量。
    /// 请注意，当前函数只对FAT12/FAT16生效。对于FAT32,返回None
    pub fn root_dir_end_bytes_offset(&self) -> Option<u64> {
        match self.bpb.fat_type {
            FATType::FAT12(_) | FATType::FAT16(_) => {
                return Some(
                    self.root_dir_bytes_offset() + (self.bpb.root_entries_cnt as u64) * 32,
                );
            }
            _ => {
                return None;
            }
        }
    }

    /// 获取簇在分区内的字节偏移量
    pub fn cluster_bytes_offset(&self, cluster: Cluster) -> u64 {
        if cluster.cluster_num >= 2 {
            // 指定簇的第一个扇区号
            let first_sec_of_cluster = (cluster.cluster_num - 2)
                * (self.bpb.sector_per_cluster as u64)
                + self.first_data_sector;
            return first_sec_of_cluster * (self.bpb.bytes_per_sector as u64);
        } else {
            return 0;
        }
    }

    /// @brief 获取一个空闲簇
    ///
    /// @param prev_cluster 簇链的前一个簇。本函数将会把新获取的簇，连接到它的后面。
    ///
    /// @return Ok(Cluster) 新获取的空闲簇
    /// @return Err(SystemError) 错误码
    pub fn allocate_cluster(&self, prev_cluster: Option<Cluster>) -> Result<Cluster, SystemError> {
        // Prepare the full-cluster zero buffer before reserving any FAT entry.
        // OOM must not leave an unreachable allocated cluster behind.
        let cluster_len = self.bytes_per_cluster() as usize;
        let mut zeros = Vec::new();
        zeros
            .try_reserve_exact(cluster_len)
            .map_err(|_| SystemError::ENOMEM)?;
        zeros.resize(cluster_len, 0);

        self.allocate_cluster_with_buffer(prev_cluster, &zeros)
    }

    pub(super) fn allocate_cluster_with_buffer(
        &self,
        prev_cluster: Option<Cluster>,
        zeros: &[u8],
    ) -> Result<Cluster, SystemError> {
        debug_assert_eq!(zeros.len(), self.bytes_per_cluster() as usize);

        let _fat_guard = self.fat_lock.lock();
        let end_cluster: Cluster = self.max_cluster_number();
        let next_free = self.fs_info.0.lock().next_free().unwrap_or(2);
        let start_cluster = Cluster::new(if (2..=end_cluster.cluster_num).contains(&next_free) {
            next_free
        } else {
            2
        });

        // 寻找一个空的簇
        let free_cluster: Cluster = match self.get_free_cluster(start_cluster, end_cluster) {
            Ok(c) => c,
            Err(SystemError::ENOSPC) if start_cluster.cluster_num > 2 => {
                self.get_free_cluster(Cluster::new(2), Cluster::new(start_cluster.cluster_num - 1))?
            }
            Err(e) => return Err(e),
        };

        // A FAT entry's FREE value is not its address. Never allow a reserved
        // cluster to reach FAT publication or cluster zeroing (volume header).
        if free_cluster.cluster_num < RESERVED_CLUSTERS as u64
            || free_cluster.cluster_num > end_cluster.cluster_num
        {
            return Err(SystemError::EIO);
        }
        if let Err(error) = self.set_entry(free_cluster, FATEntry::EndOfChain) {
            if self.set_entry(free_cluster, FATEntry::Unused).is_err() {
                // A mirrored FAT write may have reached only some copies. Do
                // not keep advertising an exact free count in that state.
                self.fs_info.0.lock().update_free_count_abs(u32::MAX);
            }
            return Err(error);
        }
        // 减少空闲簇计数
        self.fs_info.0.lock().update_free_count_delta(-1);
        // 更新搜索空闲簇的参考量
        self.fs_info
            .0
            .lock()
            .update_next_free((free_cluster.cluster_num + 1) as u32);

        // The cluster is reserved but deliberately not linked yet. Zero it
        // outside the global FAT lock, then publish the predecessor edge. A
        // failure can therefore restore this isolated entry without exposing
        // an uninitialized cluster through the file chain.
        drop(_fat_guard);
        if let Err(error) = self.zero_cluster_with_buffer(free_cluster, zeros) {
            let _fat_guard = self.fat_lock.lock();
            self.rollback_reserved_cluster_locked(free_cluster);
            return Err(error);
        }
        if let Some(prev_cluster) = prev_cluster {
            let _fat_guard = self.fat_lock.lock();
            let predecessor = match self.get_fat_entry(prev_cluster) {
                Ok(entry) => entry,
                Err(error) => {
                    self.rollback_reserved_cluster_locked(free_cluster);
                    return Err(error);
                }
            };
            if !matches!(predecessor, FATEntry::EndOfChain) {
                self.rollback_reserved_cluster_locked(free_cluster);
                return Err(SystemError::EIO);
            }
            if let Err(error) = self.set_entry(prev_cluster, FATEntry::Next(free_cluster)) {
                // set_entry may have updated only part of a mirrored FAT. It
                // is safe to free the new cluster only after the predecessor
                // is definitely restored to EOC. Otherwise retain the
                // reservation: a conservative leak is preferable to a live
                // chain pointing at a cluster that can be reallocated.
                if self.set_entry(prev_cluster, FATEntry::EndOfChain).is_ok() {
                    self.rollback_reserved_cluster_locked(free_cluster);
                } else {
                    error!(
                        "FAT: failed to restore predecessor {:?} after linking {:?}",
                        prev_cluster, free_cluster
                    );
                }
                return Err(error);
            }
        }
        return Ok(free_cluster);
    }

    /// Attach an already allocated, zeroed cluster chain to an EOC tail.
    ///
    /// The caller owns `first_cluster` as an isolated reservation. On a
    /// partial mirrored-FAT failure this method restores the predecessor when
    /// possible, but deliberately leaves the reservation allocated; the
    /// caller decides whether it is safe to release the isolated chain.
    pub(super) fn attach_reserved_chain(
        &self,
        prev_cluster: Cluster,
        first_cluster: Cluster,
    ) -> Result<(), AttachReservedError> {
        let _fat_guard = self.fat_lock.lock();
        let predecessor = self
            .get_fat_entry(prev_cluster)
            .map_err(AttachReservedError::Isolated)?;
        if !matches!(predecessor, FATEntry::EndOfChain) {
            return Err(AttachReservedError::Isolated(SystemError::EIO));
        }
        if let Err(error) = self.set_entry(prev_cluster, FATEntry::Next(first_cluster)) {
            if self.set_entry(prev_cluster, FATEntry::EndOfChain).is_err() {
                error!(
                    "FAT: failed to restore predecessor {:?} after attaching {:?}",
                    prev_cluster, first_cluster
                );
                self.fs_info.0.lock().update_free_count_abs(u32::MAX);
                return Err(AttachReservedError::Ambiguous(error));
            }
            return Err(AttachReservedError::Isolated(error));
        }
        Ok(())
    }

    /// Release a chain which has not been made reachable from a directory
    /// entry or an existing file chain. No allocation is performed here.
    pub(super) fn release_reserved_chain(&self, first_cluster: Cluster) {
        let _fat_guard = self.fat_lock.lock();
        let mut current = first_cluster;
        for _ in 0..self.max_cluster_number().cluster_num {
            let next = match self.get_fat_entry(current) {
                Ok(FATEntry::Next(next)) => Some(next),
                Ok(FATEntry::EndOfChain) => None,
                Ok(_) | Err(_) => {
                    error!("FAT: invalid reserved chain at {:?}", current);
                    return;
                }
            };
            if !self.rollback_reserved_cluster_locked(current) {
                return;
            }
            match next {
                Some(next) => current = next,
                None => return,
            }
        }
        error!("FAT: cycle detected while releasing reserved chain");
    }

    fn rollback_reserved_cluster_locked(&self, cluster: Cluster) -> bool {
        if self.set_entry(cluster, FATEntry::Unused).is_err() {
            self.fs_info.0.lock().update_free_count_abs(u32::MAX);
            error!("FAT: failed to release reserved cluster {:?}", cluster);
            return false;
        }
        let mut info = self.fs_info.0.lock();
        info.update_free_count_delta(1);
        info.update_next_free(cluster.cluster_num as u32);
        true
    }

    /// @brief 释放簇链上的所有簇
    ///
    /// @param start_cluster 簇链的第一个簇
    pub fn deallocate_cluster_chain(&self, start_cluster: Cluster) -> Result<(), SystemError> {
        let _fat_guard = self.fat_lock.lock();
        self.deallocate_cluster_chain_locked(start_cluster)
    }

    /// Detach the discarded tail before any of its clusters can be reused.
    pub(super) fn truncate_cluster_chain(
        &self,
        first: Cluster,
        keep: usize,
    ) -> Result<(), SystemError> {
        let _fat_guard = self.fat_lock.lock();
        let free_start = if keep == 0 {
            first
        } else {
            let tail = self
                .get_cluster_by_relative(first, keep - 1)
                .ok_or(SystemError::EIO)?;
            match self.get_fat_entry(tail)? {
                FATEntry::Next(next) => {
                    // On an ambiguous write error, retain the tail rather
                    // than freeing clusters that may still be reachable.
                    self.set_entry(tail, FATEntry::EndOfChain)?;
                    next
                }
                FATEntry::EndOfChain => return Ok(()),
                _ => return Err(SystemError::EIO),
            }
        };
        self.deallocate_cluster_chain_locked(free_start)
    }

    fn deallocate_cluster_chain_locked(&self, start_cluster: Cluster) -> Result<(), SystemError> {
        // Validate the whole bounded chain and reserve its release list before
        // freeing any entry. I/O failure must not look like a successful EOF
        // and a corrupt cycle must not allocate an unbounded Vec under fat_lock.
        let (count, _) = self.chain_len_and_tail(start_cluster)?;
        let count = usize::try_from(count).map_err(|_| SystemError::EOVERFLOW)?;
        let mut clusters = Vec::new();
        clusters
            .try_reserve_exact(count)
            .map_err(|_| SystemError::ENOMEM)?;
        let mut current = start_cluster;
        for index in 0..count {
            clusters.push(current);
            match self.get_fat_entry(current)? {
                FATEntry::Next(next) if index + 1 < count => current = next,
                FATEntry::EndOfChain if index + 1 == count => {}
                _ => return Err(SystemError::EIO),
            }
        }
        for c in clusters {
            self.deallocate_cluster_locked(c)?;
        }
        return Ok(());
    }

    fn deallocate_cluster_locked(&self, cluster: Cluster) -> Result<(), SystemError> {
        let entry: FATEntry = self.get_fat_entry(cluster)?;
        // 如果不是坏簇
        if entry != FATEntry::Bad {
            self.set_entry(cluster, FATEntry::Unused)?;
            self.fs_info.0.lock().update_free_count_delta(1);
            // 安全选项：清空被释放的簇
            #[cfg(feature = "fatfs-secure")]
            self.zero_cluster(cluster)?;
            return Ok(());
        } else {
            // 不能释放坏簇
            error!("Bad clusters cannot be freed.");
            return Err(SystemError::EFAULT);
        }
    }

    /// @brief 获取文件系统的根目录项
    pub fn root_dir(&self) -> FATDir {
        match self.bpb.fat_type {
            FATType::FAT32(s) => {
                return FATDir {
                    first_cluster: Cluster::new(s.root_cluster as u64),
                    dir_name: String::from("/"),
                    root_offset: None,
                    short_dir_entry: None,
                    loc: None,
                };
            }
            _ => FATDir {
                first_cluster: Cluster::new(0),
                dir_name: String::from("/"),
                root_offset: Some(self.root_dir_bytes_offset()),
                short_dir_entry: None,
                loc: None,
            },
        }
    }

    /// @brief 获取FAT表的起始扇区（相对分区起始扇区的偏移量）
    pub fn fat_start_sector(&self) -> u64 {
        let active_fat = self.active_fat();
        let fat_size = self.fat_size();
        return self.bpb.rsvd_sec_cnt as u64 + active_fat * fat_size;
    }

    /// @brief 获取当前活动的FAT表
    pub fn active_fat(&self) -> u64 {
        if self.mirroring_enabled() {
            return 0;
        } else {
            match self.bpb.fat_type {
                FATType::FAT32(bpb32) => {
                    return (bpb32.ext_flags & 0x0f) as u64;
                }
                _ => {
                    return 0;
                }
            }
        }
    }

    /// @brief 获取当前文件系统的每个FAT表的大小
    pub fn fat_size(&self) -> u64 {
        if self.bpb.fat_size_16 != 0 {
            return self.bpb.fat_size_16 as u64;
        } else {
            match self.bpb.fat_type {
                FATType::FAT32(bpb32) => {
                    return bpb32.fat_size_32 as u64;
                }

                _ => {
                    panic!("FAT12 and FAT16 volumes should have non-zero BPB_FATSz16");
                }
            }
        }
    }

    /// @brief 判断当前文件系统是否启用了FAT表镜像
    pub fn mirroring_enabled(&self) -> bool {
        match self.bpb.fat_type {
            FATType::FAT32(bpb32) => {
                return (bpb32.ext_flags & 0x80) == 0;
            }
            _ => true,
        }
    }

    /// 获取分区内的扇区偏移量
    #[inline]
    pub fn gendisk_lba_from_offset(&self, in_partition_sec_offset: u64) -> usize {
        return (in_partition_sec_offset * (self.bpb.bytes_per_sector as u64 / LBA_SIZE as u64))
            as usize;
    }

    /// @brief 获取每个扇区占用多少个LBA
    #[inline]
    pub fn lba_per_sector(&self) -> usize {
        return self.bpb.bytes_per_sector as usize / LBA_SIZE;
    }

    /// @brief 将分区内字节偏移量转换为扇区偏移量
    #[inline]
    pub fn bytes_to_sector(&self, in_partition_bytes_offset: u64) -> u64 {
        return in_partition_bytes_offset / (self.bpb.bytes_per_sector as u64);
    }

    /// @brief 根据字节偏移量计算在逻辑块内的字节偏移量
    #[inline]
    pub fn get_in_block_offset(&self, bytes_offset: u64) -> u64 {
        return bytes_offset % LBA_SIZE as u64;
    }

    /// @brief 获取在FAT表中，以start_cluster开头的FAT链的所有簇的信息
    ///
    /// @param start_cluster 整个FAT链的起始簇号
    pub fn clusters(&self, start_cluster: Cluster) -> Vec<Cluster> {
        return self.cluster_iter(start_cluster).collect();
    }

    /// @brief 获取在FAT表中，以start_cluster开头的FAT链的长度（总计经过多少个簇）
    ///
    /// @param start_cluster 整个FAT链的起始簇号
    pub fn num_clusters_chain(&self, start_cluster: Cluster) -> u64 {
        return self
            .cluster_iter(start_cluster)
            .fold(0, |size, _cluster| size + 1);
    }

    /// Ordinary metadata may read the device. Keep inode -> FAT lock order
    /// and reject malformed chains instead of swallowing iterator I/O errors.
    fn allocated_chain_sectors(&self, cluster: Cluster) -> Result<(u64, usize), SystemError> {
        let _fat = self.fat_lock.lock();
        let generation = self.fat_chain_generation.load(AtomicOrdering::Acquire);
        let (count, _) = self.chain_len_and_tail(cluster)?;
        let bytes = count
            .checked_mul(self.bytes_per_cluster())
            .ok_or(SystemError::EOVERFLOW)?;
        let sectors = usize::try_from(bytes / 512).map_err(|_| SystemError::EOVERFLOW)?;
        Ok((generation, sectors))
    }

    /// @brief 获取一个簇迭代器对象
    ///
    /// @param start_cluster 整个FAT链的起始簇号
    fn cluster_iter(&self, start_cluster: Cluster) -> ClusterIter<'_> {
        return ClusterIter {
            current_cluster: Some(start_cluster),
            fs: self,
        };
    }

    /// @brief 获取从start_cluster开始的簇链中，第n个簇的信息。（请注意，下标从0开始）
    #[inline]
    pub fn get_cluster_by_relative(&self, start_cluster: Cluster, n: usize) -> Option<Cluster> {
        self.walk_cluster(start_cluster, n as u64).ok()
    }

    /// @brief 判断FAT文件系统的shut bit是否正常。
    /// shut bit 表示文件系统是否正常卸载。如果这一位是1,则表示这个卷是“干净的”
    /// 参考资料：https://thestarman.pcministry.com/DOS/DirtyShutdownFlag.html
    ///
    /// @return Ok(true) 正常
    /// @return Ok(false) 不正常
    /// @return Err(SystemError) 在判断时发生错误
    #[allow(dead_code)]
    pub fn is_shut_bit_ok(&mut self) -> Result<bool, SystemError> {
        match self.bpb.fat_type {
            FATType::FAT32(_) => {
                // 对于FAT32, error bit位于第一个扇区的第8字节。
                let bit = self.get_fat_entry_raw(Cluster::new(1))? & 0x0800_0000;
                return Ok(bit > 0);
            }
            FATType::FAT16(_) => {
                let bit = self.get_fat_entry_raw(Cluster::new(1))? & 0x8000;
                return Ok(bit > 0);
            }
            _ => return Ok(true),
        }
    }

    /// @brief 判断FAT文件系统的hard error bit是否正常。
    /// 如果此位为0，则文件系统驱动程序在上次安装卷时遇到磁盘 I/O 错误，这表明
    /// 卷上的某些扇区可能已损坏。
    /// 参考资料：https://thestarman.pcministry.com/DOS/DirtyShutdownFlag.html
    ///
    /// @return Ok(true) 正常
    /// @return Ok(false) 不正常
    /// @return Err(SystemError) 在判断时发生错误
    #[allow(dead_code)]
    pub fn is_hard_error_bit_ok(&mut self) -> Result<bool, SystemError> {
        match self.bpb.fat_type {
            FATType::FAT32(_) => {
                let bit = self.get_fat_entry_raw(Cluster::new(1))? & 0x0400_0000;
                return Ok(bit > 0);
            }
            FATType::FAT16(_) => {
                let bit = self.get_fat_entry_raw(Cluster::new(1))? & 0x4000;
                return Ok(bit > 0);
            }
            _ => return Ok(true),
        }
    }

    /// @brief 设置文件系统的shut bit为正常状态
    /// 参考资料：https://thestarman.pcministry.com/DOS/DirtyShutdownFlag.html
    ///
    /// @return Ok(()) 设置成功
    /// @return Err(SystemError) 在设置过程中，出现错误
    pub fn set_shut_bit_ok(&self) -> Result<(), SystemError> {
        match self.bpb.fat_type {
            FATType::FAT32(_) => {
                let raw_entry = self.get_fat_entry_raw(Cluster::new(1))? | 0x0800_0000;
                self.set_entry(Cluster::new(1), FATEntry::Next(Cluster::new(raw_entry)))?;

                return Ok(());
            }

            FATType::FAT16(_) => {
                let raw_entry = self.get_fat_entry_raw(Cluster::new(1))? | 0x8000;
                self.set_entry(Cluster::new(1), FATEntry::Next(Cluster::new(raw_entry)))?;
                return Ok(());
            }
            _ => return Ok(()),
        }
    }

    /// @brief 设置文件系统的hard error bit为正常状态
    /// 参考资料：https://thestarman.pcministry.com/DOS/DirtyShutdownFlag.html
    ///
    /// @return Ok(()) 设置成功
    /// @return Err(SystemError) 在设置过程中，出现错误
    pub fn set_hard_error_bit_ok(&self) -> Result<(), SystemError> {
        match self.bpb.fat_type {
            FATType::FAT32(_) => {
                let raw_entry = self.get_fat_entry_raw(Cluster::new(1))? | 0x0400_0000;
                self.set_entry(Cluster::new(1), FATEntry::Next(Cluster::new(raw_entry)))?;
                return Ok(());
            }

            FATType::FAT16(_) => {
                let raw_entry = self.get_fat_entry_raw(Cluster::new(1))? | 0x4000;
                self.set_entry(Cluster::new(1), FATEntry::Next(Cluster::new(raw_entry)))?;
                return Ok(());
            }
            _ => return Ok(()),
        }
    }

    /// @brief 执行文件系统卸载前的一些准备工作：设置好对应的标志位，并把缓存中的数据刷入磁盘
    pub fn umount(&self) -> Result<(), SystemError> {
        self.fs_info.0.lock().flush(&self.storage)?;

        self.set_shut_bit_ok()?;

        self.set_hard_error_bit_ok()?;

        self.storage.sync()?;

        return Ok(());
    }

    fn shutdown(&self) -> Result<(), SystemError> {
        let mut holder = self.device_mount_holder.lock();
        if holder.is_none() {
            return Ok(());
        }
        let result = self.umount();
        // Release the semantic device owner before VFS publishes shutdown
        // completion. A later Drop must never write to a rebound loop device.
        drop(holder.take());
        result
    }

    /// @brief 获取文件系统的最大簇号
    pub fn max_cluster_number(&self) -> Cluster {
        match self.bpb.fat_type {
            FATType::FAT32(s) => {
                // FAT32

                // 数据扇区数量（总扇区数-保留扇区-FAT占用的扇区）
                let data_sec: u64 = self.bpb.total_sectors_32 as u64
                    - (self.bpb.rsvd_sec_cnt as u64
                        + self.bpb.num_fats as u64 * s.fat_size_32 as u64);

                // 数据区的簇数量
                let total_clusters: u64 = data_sec / self.bpb.sector_per_cluster as u64;

                // 返回最大的簇号
                return Cluster::new(total_clusters + RESERVED_CLUSTERS as u64 - 1);
            }

            _ => {
                // FAT12 / FAT16
                let root_dir_sectors: u64 = ((self.bpb.root_entries_cnt as u64) * 32)
                    .div_ceil(self.bpb.bytes_per_sector as u64);
                // 数据区扇区数
                let data_sec: u64 = self.bpb.total_sectors_16 as u64
                    - (self.bpb.rsvd_sec_cnt as u64
                        + (self.bpb.num_fats as u64 * self.bpb.fat_size_16 as u64)
                        + root_dir_sectors);
                let total_clusters = data_sec / self.bpb.sector_per_cluster as u64;
                return Cluster::new(total_clusters + RESERVED_CLUSTERS as u64 - 1);
            }
        }
    }

    /// # 清空指定的簇
    ///
    /// # 参数
    /// - cluster 要被清空的簇
    pub fn zero_cluster(&self, cluster: Cluster) -> Result<(), SystemError> {
        // 准备数据，用于写入
        let len = self.bytes_per_cluster() as usize;
        let mut zeros = Vec::new();
        zeros
            .try_reserve_exact(len)
            .map_err(|_| SystemError::ENOMEM)?;
        zeros.resize(len, 0);
        self.zero_cluster_with_buffer(cluster, &zeros)
    }

    fn zero_cluster_with_buffer(&self, cluster: Cluster, zeros: &[u8]) -> Result<(), SystemError> {
        debug_assert_eq!(zeros.len(), self.bytes_per_cluster() as usize);
        let offset = self.cluster_bytes_offset(cluster) as usize;
        super::table::write_exact_at(&self.storage, zeros, offset)?;
        Ok(())
    }
}

impl Drop for FATFileSystem {
    fn drop(&mut self) {
        let r = self.shutdown();
        if let Err(e) = r {
            error!(
                "Umount FAT filesystem failed: errno={:?}, FS detail:{self:?}",
                e
            );
        }
    }
}

impl FATFsInfo {
    const LEAD_SIG: u32 = 0x41615252;
    const STRUC_SIG: u32 = 0x61417272;
    const TRAIL_SIG: u32 = 0xAA550000;
    #[allow(dead_code)]
    const FS_INFO_SIZE: u64 = 512;

    /// @brief 从磁盘上读取FAT文件系统的FSInfo结构体
    ///
    /// @param partition 磁盘分区
    /// @param in_gendisk_fs_info_offset FSInfo扇区在gendisk内的字节偏移量（单位：字节）
    /// @param bytes_per_sec 每扇区字节数
    pub fn new(
        gendisk: &Arc<GenDisk>,
        in_gendisk_fs_info_offset: usize,
        bytes_per_sec: usize,
    ) -> Result<Self, SystemError> {
        let mut v = vec![0; bytes_per_sec];

        // 读取磁盘上的FsInfo扇区
        if gendisk.read_at(&mut v, in_gendisk_fs_info_offset / LBA_SIZE)? != v.len() {
            return Err(SystemError::EIO);
        }

        let mut cursor = VecCursor::new(v);

        let mut fsinfo = FATFsInfo {
            lead_sig: cursor.read_u32()?,
            ..Default::default()
        };
        cursor.seek(SeekFrom::SeekCurrent(480))?;
        fsinfo.struc_sig = cursor.read_u32()?;
        fsinfo.free_count = cursor.read_u32()?;
        fsinfo.next_free = cursor.read_u32()?;

        cursor.seek(SeekFrom::SeekCurrent(12))?;

        fsinfo.trail_sig = cursor.read_u32()?;
        fsinfo.dirty = false;
        fsinfo.offset = Some(in_gendisk_fs_info_offset as u64);

        if fsinfo.is_valid() {
            return Ok(fsinfo);
        } else {
            error!("Error occurred while parsing FATFsInfo.");
            return Err(SystemError::EINVAL);
        }
    }

    /// @brief 判断是否为正确的FsInfo结构体
    fn is_valid(&self) -> bool {
        self.lead_sig == Self::LEAD_SIG
            && self.struc_sig == Self::STRUC_SIG
            && self.trail_sig == Self::TRAIL_SIG
    }

    /// @brief 根据fsinfo的信息，计算当前总的空闲簇数量
    ///
    /// @param 当前文件系统的最大簇号
    #[allow(dead_code)]
    pub fn count_free_cluster(&self, max_cluster: Cluster) -> Option<u64> {
        let count_clusters = max_cluster.cluster_num - RESERVED_CLUSTERS as u64 + 1;
        // 信息不合理，当前的FsInfo中存储的free count大于计算出来的值
        if self.free_count as u64 > count_clusters {
            return None;
        } else {
            match self.free_count {
                // free count字段不可用
                0xffffffff => return None,
                // 返回FsInfo中存储的数据
                n => return Some(n as u64),
            }
        }
    }

    /// @brief 更新FsInfo中的“空闲簇统计信息“为new_count
    ///
    /// 请注意，除非手动调用`flush()`，否则本函数不会将数据刷入磁盘
    #[allow(dead_code)]
    pub fn update_free_count_abs(&mut self, new_count: u32) {
        self.free_count = new_count;
        self.dirty = true;
    }

    /// @brief 更新FsInfo中的“空闲簇统计信息“，把它加上delta.
    ///
    /// 请注意，除非手动调用`flush()`，否则本函数不会将数据刷入磁盘
    #[allow(dead_code)]
    pub fn update_free_count_delta(&mut self, delta: i32) {
        // FAT32 FSInfo 的 free_count 可能为 0xFFFFFFFF（表示不可用/未初始化）。
        // 在这种情况下不应做增量更新，否则会发生溢出并产生错误统计值。
        if self.free_count != 0xFFFF_FFFF {
            self.free_count = self
                .free_count
                .checked_add_signed(delta)
                .unwrap_or(u32::MAX);
            self.dirty = true;
        }
    }

    /// @brief 更新FsInfo中的“第一个空闲簇统计信息“为next_free.
    ///
    /// 请注意，除非手动调用`flush()`，否则本函数不会将数据刷入磁盘
    pub fn update_next_free(&mut self, next_free: u32) {
        // 这个值是参考量，不一定要准确，仅供加速查找
        self.next_free = next_free;
        self.dirty = true;
    }

    /// @brief 获取fs info 记载的第一个空闲簇。（不一定准确，仅供参考）
    pub fn next_free(&self) -> Option<u64> {
        match self.next_free {
            0xffffffff => return None,
            0 | 1 => return None,
            n => return Some(n as u64),
        };
    }

    /// @brief 把fs info刷入磁盘
    ///
    /// @param partition fs info所在的分区
    pub(super) fn flush(&mut self, gendisk: &super::io::FatStorage) -> Result<(), SystemError> {
        if !self.dirty {
            return Ok(());
        }
        if let Some(off) = self.offset {
            let in_block_offset = off % LBA_SIZE as u64;

            let lba = off as usize / LBA_SIZE;

            let mut v: Vec<u8> = vec![0; LBA_SIZE];
            if gendisk.read_at(&mut v, lba)? != v.len() {
                return Err(SystemError::EIO);
            }

            let mut cursor: VecCursor = VecCursor::new(v);
            cursor.seek(SeekFrom::SeekSet(in_block_offset as i64))?;

            cursor.write_u32(self.lead_sig)?;
            cursor.seek(SeekFrom::SeekCurrent(480))?;
            cursor.write_u32(self.struc_sig)?;
            cursor.write_u32(self.free_count)?;
            cursor.write_u32(self.next_free)?;
            cursor.seek(SeekFrom::SeekCurrent(12))?;
            cursor.write_u32(self.trail_sig)?;

            if gendisk.write_at(cursor.as_slice(), lba)? != cursor.as_slice().len() {
                return Err(SystemError::EIO);
            }
        }
        self.dirty = false;
        return Ok(());
    }

    /// @brief 读取磁盘上的Fs Info扇区，将里面的内容更新到结构体中
    ///
    /// @param partition fs info所在的分区
    #[allow(dead_code)]
    pub fn update(&mut self, partition: Arc<Partition>) -> Result<(), SystemError> {
        if let Some(off) = self.offset {
            let in_block_offset = off % LBA_SIZE as u64;

            let lba = off as usize / LBA_SIZE;

            let mut v: Vec<u8> = vec![0; LBA_SIZE];
            let device_lba = partition
                .lba_start
                .checked_add(lba as u64)
                .and_then(|value| usize::try_from(value).ok())
                .ok_or(SystemError::EOVERFLOW)?;
            if partition.disk().read_at(device_lba, 1, &mut v)? != v.len() {
                return Err(SystemError::EIO);
            }
            let mut cursor: VecCursor = VecCursor::new(v);
            cursor.seek(SeekFrom::SeekSet(in_block_offset as i64))?;
            self.lead_sig = cursor.read_u32()?;

            cursor.seek(SeekFrom::SeekCurrent(480))?;
            self.struc_sig = cursor.read_u32()?;
            self.free_count = cursor.read_u32()?;
            self.next_free = cursor.read_u32()?;
            cursor.seek(SeekFrom::SeekCurrent(12))?;
            self.trail_sig = cursor.read_u32()?;
        }
        return Ok(());
    }
}

impl LockedFATInode {
    /// Shared legacy blockdev DIO admission for original user iterators and
    /// kernel CPU slices. EOF precedes whole-iterator alignment checks.
    fn direct_read_length(
        &self,
        offset: usize,
        len: usize,
        iterator_alignment: usize,
    ) -> Result<usize, SystemError> {
        if len == 0 {
            return Ok(0);
        }
        let (size, alignment) = {
            let _size = self.1.read();
            let cache = self.0.lock().page_cache.clone();
            if let Some(cache) = cache {
                let end = offset.checked_add(len).ok_or(SystemError::EOVERFLOW)?;
                cache.manager().writeback_range(
                    offset >> MMArch::PAGE_SHIFT,
                    (end - 1) >> MMArch::PAGE_SHIFT,
                )?;
            }
            let guard = self.0.lock();
            let fs = guard.fs.upgrade().ok_or(SystemError::EIO)?;
            let size = match &guard.inode_type {
                FATDirEntry::File(file) | FATDirEntry::VolId(file) => file.size(),
                FATDirEntry::Dir(_) => return Err(SystemError::EISDIR),
                FATDirEntry::UnInit => return Err(SystemError::EROFS),
            };
            (
                usize::try_from(size).map_err(|_| SystemError::EOVERFLOW)?,
                1usize << fs.storage.block_size_log2(),
            )
        };
        // Legacy blockdev_direct_IO checks EOF before its whole-iterator OR
        // alignment; unlike iomap, empty vector addresses do not contribute.
        if offset >= size {
            return Ok(0);
        }
        if (offset | iterator_alignment) & (alignment - 1) != 0 {
            return Err(SystemError::EINVAL);
        }
        Ok(len.min(size - offset))
    }

    fn try_read_pagecache(
        &self,
        offset: usize,
        buf: &mut [u8],
    ) -> Result<usize, MemoryAllocationError> {
        let page_cache = self.0.lock().page_cache.clone();
        if let Some(page_cache) = page_cache {
            let task = crate::process::ProcessManager::current_pcb();
            let owner = crate::cgroup::memory::account_for_task(&task);
            let creator = crate::cgroup::memory::object_for_task(&task);
            // Preserve admission scope for the lock-free File/NOIO retry
            // boundary; the legacy cache wrapper would erase Charge here.
            return page_cache.try_read(offset, buf, &owner, &creator);
        } else {
            return self.read_sync(offset, buf).map_err(Into::into);
        }
    }

    fn try_write_pagecache(
        &self,
        offset: usize,
        buf: &[u8],
    ) -> Result<usize, MemoryAllocationError> {
        let _size_guard = self.1.read();
        if !buf.is_empty() {
            self.0.lock().touch_modified()?;
        }
        self.try_write_pagecache_locked(offset, buf)
    }

    /// Caller owns the inode size read transaction.
    fn try_write_pagecache_locked(
        &self,
        offset: usize,
        buf: &[u8],
    ) -> Result<usize, MemoryAllocationError> {
        let page_cache = self.0.lock().page_cache.clone();
        if let Some(page_cache) = page_cache {
            let task = crate::process::ProcessManager::current_pcb();
            let owner = crate::cgroup::memory::account_for_task(&task);
            let creator = crate::cgroup::memory::object_for_task(&task);
            let write_len = page_cache.try_write(offset, buf, &owner, &creator)?;
            let mut guard = self.0.lock();
            let old_size = guard.metadata.size;
            guard.update_metadata(Some(core::cmp::max(old_size, (offset + write_len) as i64)));
            return Ok(write_len);
        } else {
            return self.write_sync(offset, buf).map_err(Into::into);
        }
    }
}

impl LockedFATInode {
    fn write_direct_data_locked(&self, offset: usize, bytes: &[u8]) -> Result<usize, SystemError> {
        if bytes.is_empty() {
            return Ok(0);
        }
        self.with_clean_direct_range(offset, bytes.len(), || self.write_sync(offset, bytes))
    }

    /// A single clean-cache publication boundary for native DIO and the
    /// post-write buffered fallback. Flush errors are terminal SystemError:
    /// data has already been published and must never enter charge replay.
    fn with_clean_direct_range<T>(
        &self,
        offset: usize,
        len: usize,
        publish: impl FnOnce() -> Result<T, SystemError>,
    ) -> Result<T, SystemError> {
        let end = offset.checked_add(len).ok_or(SystemError::EFBIG)?;
        let first = offset >> MMArch::PAGE_SHIFT;
        let last = (end - 1) >> MMArch::PAGE_SHIFT;
        let cache = self.0.lock().page_cache.clone().ok_or(SystemError::EIO)?;
        loop {
            cache.manager().writeback_range(first, last)?;
            cache.unmap_mapping_pages(first, Some(last + 1))?;
            let invalidate = cache.invalidate_write();
            if cache.has_dirty_or_writeback_pages_in_range(first, last) {
                drop(invalidate);
                continue;
            }
            match cache.try_discard_range_locked(first, last + 1)? {
                crate::filesystem::page_cache::PageCacheRangeResult::Retry(retry) => {
                    drop(invalidate);
                    retry.wait()?;
                    continue;
                }
                crate::filesystem::page_cache::PageCacheRangeResult::Ready(()) => {}
            }
            return publish();
        }
    }
}

impl IndexNode for LockedFATInode {
    fn write_primary_with_privilege_stage(
        &self,
        offset: usize,
        len: usize,
        source: crate::filesystem::vfs::PrimaryWriteSource<'_, '_>,
        intent: crate::filesystem::vfs::WriteOperation,
        data: MutexGuard<FilePrivateData>,
        stage: &mut crate::filesystem::vfs::WritePrivilegeStage<'_>,
    ) -> Result<crate::filesystem::vfs::PrimaryWriteResult, MemoryAllocationError> {
        drop(data);
        if len == 0 {
            return Ok(crate::filesystem::vfs::PrimaryWriteResult::new(0));
        }
        let end = offset.checked_add(len).ok_or(SystemError::EFBIG)?;
        let _size = self.1.read();
        let (frontier, alignment) = {
            let guard = self.0.lock();
            let fs = guard.fs.upgrade().ok_or(SystemError::EIO)?;
            let size = match &guard.inode_type {
                FATDirEntry::File(file) | FATDirEntry::VolId(file) => file.size(),
                FATDirEntry::Dir(_) => return Err(SystemError::EISDIR.into()),
                FATDirEntry::UnInit => return Err(SystemError::EROFS.into()),
            };
            (size, 1usize << fs.storage.block_size_log2())
        };
        // As in fat_direct_IO(), mappings that would need chain expansion
        // fall back for the complete request. Cached EOF is not allocation.
        let direct = matches!(
            intent.io,
            crate::filesystem::vfs::WriteIoIntent::Direct { .. }
        ) && end as u64 <= frontier;
        stage.complete();
        // One file_modified stage before content or user-copy progress. A
        // timestamp I/O error cannot erase a later positive write prefix.
        self.0.lock().touch_modified()?;
        if direct {
            intent.io.check_alignment(offset | len, alignment)?;
        }
        let written = source.write_chunks(len, |relative, bytes| {
            let position = offset.checked_add(relative).ok_or(SystemError::EFBIG)?;
            if direct {
                let count = bytes.len() & !(alignment - 1);
                if count == 0 {
                    return Err(SystemError::EFAULT.into());
                }
                self.write_direct_data_locked(position, &bytes[..count])
                    .map_err(Into::into)
            } else {
                self.try_write_pagecache_locked(position, bytes)
            }
        })?;
        if !direct
            && written != 0
            && matches!(
                intent.io,
                crate::filesystem::vfs::WriteIoIntent::Direct { .. }
            )
        {
            self.with_clean_direct_range(offset, written, || Ok(()))?;
        }
        Ok(crate::filesystem::vfs::PrimaryWriteResult::new(written))
    }

    fn configure_open_file(
        &self,
        _data: &FilePrivateData,
        behavior: &mut crate::filesystem::vfs::file::OpenFileBehavior,
    ) {
        if behavior.data_file_write_checks {
            behavior.direct_io = crate::filesystem::vfs::file::DirectIoPolicy::Native;
        }
    }

    fn retention_state(&self) -> Option<&InodeRetentionState> {
        Some(&self.3.retention)
    }

    fn on_zero_retention(&self) {
        self.schedule_reclaim();
    }

    fn link_mutation_coordinator(&self) -> Option<&LinkMutationCoordinator> {
        Some(&self.2)
    }

    fn append_lock_fs(&self) -> Option<Arc<dyn FileSystem>> {
        Some(self.fs())
    }

    fn mmap(&self, _start: usize, _len: usize, _offset: usize) -> Result<(), SystemError> {
        Ok(())
    }

    fn sync_file(
        &self,
        datasync: bool,
        data: MutexGuard<FilePrivateData>,
    ) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
        self.sync_file_range(0, usize::MAX, datasync, data)
    }

    fn sync_file_range(
        &self,
        start: usize,
        end: usize,
        _datasync: bool,
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
        let result: core::result::Result<_, system_error::SystemError> = (|| {
            let (file_type, page_cache, fs) = {
                let inode = self.0.lock();
                (
                    inode.metadata.file_type,
                    inode.page_cache.clone(),
                    inode.fs.upgrade().ok_or(SystemError::EIO)?,
                )
            };

            if !matches!(file_type, FileType::File | FileType::Dir) {
                return Err(SystemError::EINVAL);
            }

            if let Some(page_cache) = page_cache {
                let start_index = start >> MMArch::PAGE_SHIFT;
                let end_index = end >> MMArch::PAGE_SHIFT;
                // writeback_range() is synchronous: it submits and waits for the
                // selected dirty pages, propagating submission/writeback errors.
                // FATFile::write()/ensure_len() also submits the FAT chain and
                // short direntry updates needed to make an extended file
                // reachable before this call returns.
                page_cache
                    .manager()
                    .writeback_range(start_index, end_index)?;
            }

            // FAT cannot safely omit size/cluster metadata for fdatasync, so both
            // fsync and fdatasync share this final power-loss durability boundary.
            // Do not move this barrier into page writeback: a single file-level
            // sync must cover the related data, FAT chain, and directory entry,
            // and the device flush error must be visible to the caller.
            fs.storage.sync()
        })();
        result.map_err(Into::into)
    }

    fn read_sync(&self, offset: usize, buf: &mut [u8]) -> Result<usize, SystemError> {
        let mut guard: MutexGuard<FATInode> = self.0.lock();
        let fs = guard.fs.upgrade().unwrap();
        match &mut guard.inode_type {
            FATDirEntry::File(f) | FATDirEntry::VolId(f) => {
                let r = f.read(&fs, buf, offset as u64);
                return r;
            }

            FATDirEntry::Dir(_) => {
                return Err(SystemError::EISDIR);
            }
            FATDirEntry::UnInit => {
                error!("FATFS: param: Inode_type uninitialized.");
                return Err(SystemError::EROFS);
            }
        }
    }

    fn write_sync(&self, offset: usize, buf: &[u8]) -> Result<usize, SystemError> {
        let mut guard: MutexGuard<FATInode> = self.0.lock();
        let fs: &Arc<FATFileSystem> = &guard.fs.upgrade().unwrap();

        match &mut guard.inode_type {
            FATDirEntry::File(f) | FATDirEntry::VolId(f) => {
                let r = f.write(fs, buf, offset as u64);
                return r;
            }

            FATDirEntry::Dir(_) => {
                return Err(SystemError::EISDIR);
            }

            FATDirEntry::UnInit => {
                error!("FATFS: param: Inode_type uninitialized.");
                return Err(SystemError::EROFS);
            }
        }
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
        let len = core::cmp::min(len, buf.len());
        self.try_read_pagecache(offset, &mut buf[0..len])
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
        let len = core::cmp::min(len, buf.len());
        self.try_write_pagecache(offset, &buf[0..len])
    }

    fn read_at_with_io_intent(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        data: MutexGuard<FilePrivateData>,
        intent: crate::filesystem::vfs::ReadIoIntent,
    ) -> Result<usize, MemoryAllocationError> {
        if intent != crate::filesystem::vfs::ReadIoIntent::Direct {
            return self.read_at(offset, len, buf, data);
        }
        drop(data);
        let len = len.min(buf.len());
        let len = self.direct_read_length(offset, len, len)?;
        if len == 0 {
            return Ok(0);
        }
        let _size = self.1.read();
        self.read_sync(offset, &mut buf[..len]).map_err(Into::into)
    }

    fn read_user_with_io_intent(
        &self,
        offset: usize,
        len: usize,
        writer: &mut crate::syscall::user_buffer::UserBuffer<'_>,
        data: MutexGuard<FilePrivateData>,
        intent: crate::filesystem::vfs::ReadIoIntent,
    ) -> Result<Option<usize>, SystemError> {
        if intent != crate::filesystem::vfs::ReadIoIntent::Direct {
            return self.read_user_at(offset, len, writer, data);
        }
        drop(data);
        let len = len.min(writer.len());
        if len == 0 {
            return Ok(Some(0));
        }
        let len = self.direct_read_length(offset, len, writer.alignment(len))?;
        let mut scratch = crate::filesystem::vfs::read_buffer::ReadScratch::default();
        let mut total = 0;
        while total < len {
            let want = (len - total).min(crate::filesystem::vfs::read_buffer::MAX_CHUNK);
            let buffer = scratch.prepare(want)?;
            let read = {
                let _size = self.1.read();
                self.read_sync(offset + total, buffer)?
            };
            if read == 0 {
                break;
            }
            // All FAT content/size locks are gone before a destination fault.
            let copied = match writer.write_prefix_to_user(total, &buffer[..read]) {
                Ok(copied) => copied,
                Err(SystemError::EFAULT) if total != 0 => return Ok(Some(total)),
                Err(error) => return Err(error),
            };
            total += copied;
            if copied != read {
                // Legacy FAT DIO / a completed raw-device BIO retain the
                // bytes transferred before a later destination-page fault.
                return Ok(Some(total));
            }
        }
        Ok(Some(total))
    }

    fn read_direct(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let len = core::cmp::min(len, buf.len());
        let r = self.read_sync(offset, &mut buf[0..len]);
        // self.0.lock_irqsave().update_metadata();
        return r;
    }

    fn write_direct(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let len = core::cmp::min(len, buf.len());
        let _size = self.1.read();
        if len != 0 {
            self.0.lock().touch_modified()?;
        }
        self.write_direct_data_locked(offset, &buf[..len])
    }

    fn create(
        &self,
        name: &str,
        file_type: FileType,
        _mode: InodeMode,
    ) -> Result<Arc<dyn IndexNode>, crate::mm::allocation::MemoryAllocationError> {
        // 由于FAT32不支持文件权限的功能，因此忽略mode参数
        let mut guard: MutexGuard<FATInode> = self.0.lock();
        let fs: &Arc<FATFileSystem> = &guard.fs.upgrade().unwrap();

        match &mut guard.inode_type {
            FATDirEntry::File(_) | FATDirEntry::VolId(_) => {
                return Err(SystemError::ENOTDIR.into());
            }
            FATDirEntry::Dir(d) => match file_type {
                FileType::File => {
                    d.create_file(name, fs)?;
                    guard.invalidate_negative_children();
                    let child = guard.find(name)?;
                    guard.touch_modified()?;
                    return Ok(child);
                }
                FileType::Dir => {
                    d.create_dir(name, fs)?;
                    guard.invalidate_negative_children();
                    let child = guard.find(name)?;
                    // 刚创建的目录，确保自身 nlink >= 2，并更新父目录 nlink
                    let mut child_md = child.0.lock();
                    if child_md.metadata.nlinks < 2 {
                        child_md.metadata.nlinks = 2;
                    }
                    drop(child_md);
                    // 父目录因为新增子目录，多一个链接（来自子目录的 ".."）
                    guard.metadata.nlinks += 1;
                    guard.touch_modified()?;
                    return Ok(child);
                }

                FileType::SymLink => return Err(SystemError::ENOSYS.into()),

                FileType::Socket => {
                    d.create_file(name, fs)?;
                    guard.invalidate_negative_children();
                    let child = guard.find(name)?;
                    guard.touch_modified()?;
                    return Ok(child);
                }

                _ => return Err(SystemError::EINVAL.into()),
            },
            FATDirEntry::UnInit => {
                error!("FATFS: param: Inode_type uninitialized.");
                return Err(SystemError::EROFS.into());
            }
        }
    }

    // fat32 does not support hard link
    fn link(
        &self,
        _name: &str,
        _other: &Arc<dyn IndexNode>,
    ) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
        let result: core::result::Result<_, system_error::SystemError> =
            Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        result.map_err(Into::into)
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        return self.0.lock().fs.upgrade().unwrap();
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        return self;
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        self.0.lock().metadata_with_allocated_blocks()
    }

    fn birth_time(&self, metadata: &Metadata) -> Option<PosixTimeSpec> {
        Some(metadata.btime)
    }
    fn set_metadata(
        &self,
        metadata: &Metadata,
    ) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
        self.0
            .lock()
            .apply_metadata(
                metadata,
                SetMetadataMask::ATIME
                    | SetMetadataMask::MTIME
                    | SetMetadataMask::CTIME
                    | SetMetadataMask::MODE
                    | SetMetadataMask::UID
                    | SetMetadataMask::GID,
            )
            .map_err(Into::into)
    }

    fn set_metadata_masked(
        &self,
        metadata: &Metadata,
        mask: SetMetadataMask,
    ) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
        self.0
            .lock()
            .apply_metadata(metadata, mask)
            .map_err(Into::into)
    }

    fn update_atime(&self, now: PosixTimeSpec, relatime: bool) -> Result<(), SystemError> {
        let mut inode = self.0.lock();
        if inode
            .fs
            .upgrade()
            .ok_or(SystemError::EIO)?
            .storage
            .check_writable()
            .is_err()
        {
            return Ok(());
        }
        let mut proposed = inode.metadata.clone();
        if crate::filesystem::vfs::update_atime_locked(&mut proposed, now, relatime) {
            inode.apply_metadata(&proposed, SetMetadataMask::ATIME)?;
        }
        Ok(())
    }

    fn resize(&self, len: usize) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
        let result: core::result::Result<_, system_error::SystemError> = (|| {
            let _size_guard = self.1.write();
            //检查是否超过fat支持的最大容量
            if (len as u64) > MAX_FILE_SIZE {
                return Err(SystemError::EFBIG);
            }
            let mut guard: MutexGuard<FATInode> = self.0.lock();
            let fs: &Arc<FATFileSystem> = &guard.fs.upgrade().unwrap();
            let old_size = guard.metadata.size as usize;
            let page_cache = guard.page_cache.clone();

            match &mut guard.inode_type {
                FATDirEntry::File(file) | FATDirEntry::VolId(file) => {
                    // 如果新的长度和旧的长度相同，那么就直接返回
                    match len.cmp(&old_size) {
                        Ordering::Equal => {
                            return Ok(());
                        }
                        Ordering::Greater => {
                            // 如果新的长度比旧的长度大，那么就在文件末尾添加空白
                            let buf: Vec<u8> = vec![0u8; ZERO_BUF_SIZE];

                            let mut remain_size = len - old_size;
                            let mut offset = old_size;
                            while remain_size > 0 {
                                let write_size = core::cmp::min(remain_size, ZERO_BUF_SIZE);
                                file.write(fs, &buf[..write_size], offset as u64)?;
                                remain_size -= write_size;
                                offset += write_size;
                            }
                            // Unlike ordinary/O_SYNC writes, resize and mode-0
                            // fallocate do not pass through File post-write sync.
                            // Match Linux fat_cont_expand() only for IS_SYNC.
                            guard.synchronize_metadata();
                            guard.metadata.size = len as i64;
                            let inode_sync = guard.metadata.flags.contains(InodeFlags::S_SYNC);
                            let fs = fs.clone();
                            drop(guard);
                            let mounted_fs: Arc<dyn FileSystem> = fs.clone();
                            if inode_sync || filesystem_is_synchronous(&mounted_fs) {
                                fs.storage.sync()?;
                            }
                            return Ok(());
                        }
                        Ordering::Less => {
                            guard.metadata.size = len as i64;
                            drop(guard);
                            if let Some(page_cache) = page_cache {
                                if let Err(error) = page_cache.manager().resize(len) {
                                    self.0.lock().synchronize_metadata();
                                    return Err(error);
                                }
                            }
                            let mut guard: MutexGuard<FATInode> = self.0.lock();
                            let fs: &Arc<FATFileSystem> = &guard.fs.upgrade().unwrap();
                            match &mut guard.inode_type {
                                FATDirEntry::File(file) | FATDirEntry::VolId(file) => {
                                    let result = file.truncate(fs, len as u64);
                                    guard.synchronize_metadata();
                                    return result;
                                }
                                FATDirEntry::Dir(_) => return Err(SystemError::ENOSYS),
                                FATDirEntry::UnInit => {
                                    error!("FATFS: param: Inode_type uninitialized.");
                                    return Err(SystemError::EROFS);
                                }
                            }
                        }
                    }
                }
                FATDirEntry::Dir(_) => return Err(SystemError::ENOSYS),
                FATDirEntry::UnInit => {
                    error!("FATFS: param: Inode_type uninitialized.");
                    return Err(SystemError::EROFS);
                }
            }
        })();
        result.map_err(Into::into)
    }

    fn fallocate_resize_atomic(
        &self,
        _offset: usize,
        requested_end: usize,
        _lock_owner: u64,
    ) -> Result<SetMetadataMask, crate::mm::allocation::MemoryAllocationError> {
        let _size_guard = self.1.write();
        if requested_end as u64 > MAX_FILE_SIZE {
            return Err(SystemError::EFBIG.into());
        }
        let mut guard = self.0.lock();
        let old_size = guard.metadata.size.max(0) as usize;
        if requested_end <= old_size {
            // Linux FAT treats mode-0 fallocate inside i_size as a successful
            // no-op. The VFS still emits MODIFY, but FAT does not run
            // file_modified or update cmtime here.
            return Ok(SetMetadataMask::empty());
        }
        crate::filesystem::vfs::vcore::check_file_size_limit(requested_end)?;
        let fs = guard.fs.upgrade().ok_or(SystemError::EIO)?;
        match &mut guard.inode_type {
            FATDirEntry::File(file) | FATDirEntry::VolId(file) => {
                // Allocate and zero the complete gap before publishing the
                // new length. Unlike chunked `write()`, ensure_len performs a
                // single size commit, so a later chunk error cannot expose a
                // partially advanced i_size for a failed fallocate.
                file.extend_zeroed(&fs, requested_end as u64, requested_end as u64)?;
            }
            FATDirEntry::Dir(_) => return Err(SystemError::ENOSYS.into()),
            FATDirEntry::UnInit => return Err(SystemError::EROFS.into()),
        }
        let (metadata, mut mask) =
            crate::filesystem::vfs::vcore::prepare_write_side_effect_metadata(
                guard.metadata.clone(),
                requested_end,
            );
        // Linux FAT's mode-0 fallocate does not call file_modified(), so it
        // neither performs killpriv nor publishes ATTRIB for a mode change.
        mask.remove(SetMetadataMask::MODE);
        guard.metadata.size = requested_end as i64;
        guard.synchronize_metadata();
        guard.apply_metadata(&metadata, mask)?;
        let inode_sync = guard.metadata.flags.contains(InodeFlags::S_SYNC);
        drop(guard);
        let mounted_fs: Arc<dyn FileSystem> = fs.clone();
        if inode_sync || filesystem_is_synchronous(&mounted_fs) {
            fs.storage.sync()?;
        }
        Ok(mask)
    }

    fn fallocate_file(
        &self,
        mode: i32,
        offset: usize,
        len: usize,
        lock_owner: u64,
        attrib: &mut crate::filesystem::vfs::AttribStageObserver<'_>,
        data: MutexGuard<FilePrivateData>,
    ) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
        drop(data);
        crate::filesystem::vfs::vcore::resize_based_fallocate(
            self, mode, offset, len, lock_owner, attrib,
        )
    }

    fn truncate(&self, len: usize) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
        let guard: MutexGuard<FATInode> = self.0.lock();
        let old_size = guard.metadata.size as usize;
        if len < old_size {
            drop(guard);
            self.resize(len)
        } else {
            Ok(())
        }
    }

    fn list(&self) -> Result<Vec<String>, SystemError> {
        let mut guard: MutexGuard<FATInode> = self.0.lock();
        let fatent: &FATDirEntry = &guard.inode_type;
        match fatent {
            FATDirEntry::File(_) | FATDirEntry::VolId(_) => {
                return Err(SystemError::ENOTDIR);
            }
            FATDirEntry::Dir(dir) => {
                // 获取当前目录下的所有目录项
                let mut ret: Vec<String> = Vec::new();
                let dir_iter: FATDirIter = dir.to_iter(guard.fs.upgrade().unwrap());
                for ent in dir_iter {
                    let ent = ent?;
                    ret.push(ent.name());

                    // ====== 生成inode缓存
                    let search_name = to_search_name_string(ent.name());
                    // debug!("name={name}");

                    if !guard.children.contains_key(&search_name)
                        && search_name != "."
                        && search_name != ".."
                    {
                        let name = DName::from(ent.name());
                        // 创建新的inode
                        let entry_inode: Arc<LockedFATInode> = LockedFATInode::new(
                            name.clone(),
                            guard.fs.upgrade().unwrap(),
                            guard.self_ref.clone(),
                            ent,
                        )?;
                        // 加入缓存区, 由于FAT文件系统的大小写不敏感问题，因此存入缓存区的key应当是全大写的
                        guard.negative_children.pop(&search_name);
                        guard.children.insert(search_name, entry_inode.clone());
                    }
                }
                return Ok(ret);
            }
            FATDirEntry::UnInit => {
                error!("FATFS: param: Inode_type uninitialized.");
                return Err(SystemError::EROFS);
            }
        }
    }

    fn find(&self, name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        let mut guard: MutexGuard<FATInode> = self.0.lock();
        let target = guard.find(name)?;
        return Ok(target);
    }

    fn open(
        &self,
        _data: MutexGuard<FilePrivateData>,
        _flags: &FileFlags,
    ) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
        let result: core::result::Result<_, system_error::SystemError> = (|| {
            return Ok(());
        })();
        result.map_err(Into::into)
    }

    fn close(&self, _data: MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        return Ok(());
    }

    fn unlink(
        &self,
        name: &str,
    ) -> Result<LinkRemovalOutcome, crate::mm::allocation::MemoryAllocationError> {
        let mut guard = self.0.lock();
        let target = guard.find(name)?;
        let file_type = target.0.lock().metadata.file_type;
        if file_type == FileType::Pipe {
            let canonical_name = target.0.lock().dname.0.clone();
            guard.mark_child_absent(name, &canonical_name);
            return Ok(LinkRemovalOutcome::LastLink);
        }
        let fs = guard.fs.upgrade().ok_or(SystemError::EIO)?;
        let dir = match &guard.inode_type {
            FATDirEntry::Dir(dir) => dir,
            _ => return Err(SystemError::ENOTDIR.into()),
        };
        dir.check_existence(name, Some(false), fs.clone())?;
        let removed = target.remove_file_name(dir, fs, name);
        let detached = {
            let mut victim = target.0.lock();
            if victim.metadata.nlinks == 0 {
                victim.touch_deleted()?;
                true
            } else {
                false
            }
        };
        if detached {
            let canonical_name = target.0.lock().dname.0.clone();
            guard.mark_child_absent(name, &canonical_name);
            let time = guard.touch_modified();
            removed.and(time)?;
        } else {
            removed?;
        }
        Ok(LinkRemovalOutcome::LastLink)
    }

    fn rmdir(&self, name: &str) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
        let mut guard = self.0.lock();
        let target = guard.find(name)?;
        let fs = guard.fs.upgrade().ok_or(SystemError::EIO)?;
        let dir = match &guard.inode_type {
            FATDirEntry::Dir(dir) => dir,
            FATDirEntry::UnInit => return Err(SystemError::EROFS.into()),
            _ => return Err(SystemError::ENOTDIR.into()),
        };
        let removed = target.remove_directory_name(dir, fs, name);
        let detached = {
            let mut victim = target.0.lock();
            if victim.metadata.nlinks == 0 {
                victim.touch_deleted()?;
                true
            } else {
                false
            }
        };
        if detached {
            guard.metadata.nlinks = guard.metadata.nlinks.saturating_sub(1);
            let canonical_name = target.0.lock().dname.0.clone();
            guard.mark_child_absent(name, &canonical_name);
            let time = guard.touch_modified();
            return removed.and(time).map_err(Into::into);
        }
        removed.map_err(Into::into)
    }

    fn move_to(
        &self,
        old_name: &str,
        target: &Arc<dyn IndexNode>,
        new_name: &str,
        flags: RenameFlags,
    ) -> Result<RenameOutcome, crate::mm::allocation::MemoryAllocationError> {
        let old_id = self.metadata().unwrap().inode_id;
        let new_id = target.metadata().unwrap().inode_id;
        // 若在同一父目录下
        if old_id == new_id {
            self.rename_file_in_current_dir(old_name, new_name, flags)
                .map_err(Into::into)
        } else {
            self.move_to_another_dir(old_name, new_name, target, flags)
                .map_err(Into::into)
        }
    }

    fn get_entry_name(&self, ino: InodeId) -> Result<String, SystemError> {
        let guard: MutexGuard<FATInode> = self.0.lock();
        if guard.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }
        match ino.into() {
            0 => {
                return Ok(String::from("."));
            }
            1 => {
                return Ok(String::from(".."));
            }
            ino => {
                // 暴力遍历所有的children，判断inode id是否相同
                // TODO: 优化这里，这个地方性能很差！
                let mut key: Vec<String> = guard
                    .children
                    .iter()
                    .filter_map(|(k, v)| {
                        if v.0.lock().metadata.inode_id.into() == ino {
                            Some(k.to_string())
                        } else {
                            None
                        }
                    })
                    .collect();

                match key.len() {
                    0 => {
                        return Err(SystemError::ENOENT);
                    }
                    1 => {
                        return Ok(key.remove(0));
                    }
                    _ => panic!(
                        "FatFS get_entry_name: key.len()={key_len}>1, current inode_id={inode_id:?}, to find={to_find:?}",
                        key_len = key.len(),
                        inode_id = guard.metadata.inode_id,
                        to_find = ino
                    ),
                }
            }
        }
    }

    fn mknod(
        &self,
        filename: &str,
        mode: InodeMode,
        _dev_t: DeviceNumber,
    ) -> Result<Arc<dyn IndexNode>, crate::mm::allocation::MemoryAllocationError> {
        let mut inode = self.0.lock();
        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR.into());
        }

        let mode = if (mode.bits() & InodeMode::S_IFMT.bits()) == 0 {
            mode | InodeMode::S_IFREG
        } else {
            mode
        };
        let umask = crate::process::ProcessManager::current_pcb()
            .fs_struct()
            .umask();
        let final_mode = mode & !umask;

        let file_type = FileType::from(final_mode);
        // 判断需要创建的类型
        if unlikely(file_type == FileType::File) {
            // 普通文件
            drop(inode);
            return self.create(filename, FileType::File, mode);
        }

        let dname = DName::from(filename);
        let nod = LockedFATInode::new(
            dname,
            inode.fs.upgrade().unwrap(),
            inode.self_ref.clone(),
            FATDirEntry::File(FATFile::default()),
        )?;

        if file_type == FileType::Pipe {
            nod.0.lock().metadata.file_type = FileType::Pipe;
            // 创建pipe文件
            let pipe_inode = LockedPipeInode::new_fifo_shell();
            // 标记为命名管道（FIFO），这样 open 时才会应用 FIFO 阻塞语义
            pipe_inode.set_fifo();
            // 设置special_node
            nod.0.lock().special_node = Some(SpecialNodeData::Pipe(pipe_inode));
        } else if file_type == FileType::Socket {
            nod.0.lock().metadata.file_type = FileType::Socket;
        } else if file_type == FileType::BlockDevice {
            nod.0.lock().metadata.file_type = FileType::BlockDevice;
            unimplemented!()
        } else if file_type == FileType::CharDevice {
            nod.0.lock().metadata.file_type = FileType::CharDevice;
            unimplemented!()
        } else {
            return Err(SystemError::EINVAL.into());
        }

        let search_name = to_search_name(filename);
        inode.invalidate_negative_children();
        inode.children.insert(search_name, nod.clone());
        Ok(nod)
    }

    fn special_node(&self) -> Option<SpecialNodeData> {
        self.0.lock().special_node.clone()
    }

    fn dname(&self) -> Result<DName, SystemError> {
        Ok(self.0.lock().dname.clone())
    }

    fn parent(&self) -> Result<Arc<dyn IndexNode>, SystemError> {
        self.0
            .lock()
            .parent
            .upgrade()
            .map(|item| item as Arc<dyn IndexNode>)
            .ok_or(SystemError::EINVAL)
    }

    fn page_cache(&self) -> Option<Arc<PageCache>> {
        self.0.lock().page_cache.clone()
    }
}

impl Default for FATFsInfo {
    fn default() -> Self {
        return FATFsInfo {
            lead_sig: FATFsInfo::LEAD_SIG,
            struc_sig: FATFsInfo::STRUC_SIG,
            free_count: 0xFFFFFFFF,
            next_free: RESERVED_CLUSTERS,
            trail_sig: FATFsInfo::TRAIL_SIG,
            dirty: false,
            offset: None,
        };
    }
}

impl Cluster {
    pub fn new(cluster: u64) -> Self {
        return Cluster {
            cluster_num: cluster,
            parent_cluster: 0,
        };
    }
}

/// @brief 用于迭代FAT表的内容的簇迭代器对象
#[derive(Debug)]
struct ClusterIter<'a> {
    /// 迭代器的next要返回的簇
    current_cluster: Option<Cluster>,
    /// 属于的文件系统
    fs: &'a FATFileSystem,
}

impl Iterator for ClusterIter<'_> {
    type Item = Cluster;

    fn next(&mut self) -> Option<Self::Item> {
        // 当前要返回的簇
        let ret: Option<Cluster> = self.current_cluster;

        // 获得下一个要返回簇
        let new: Option<Cluster> = match self.current_cluster {
            Some(c) => {
                let entry: Option<FATEntry> = self.fs.get_fat_entry(c).ok();
                match entry {
                    Some(FATEntry::Next(c)) => Some(c),
                    _ => None,
                }
            }
            _ => None,
        };

        self.current_cluster = new;
        return ret;
    }
}
