use core::any::Any;
use core::fmt::Write;
use core::intrinsics::unlikely;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::filesystem::page_cache::{PageCache, PageCacheBackend};
use crate::filesystem::vfs::syscall::RenameFlags;
use crate::filesystem::vfs::{FileSystemMakerData, FSMAKER};
use crate::libs::rwsem::RwSem;
use crate::mm::allocator::page_frame::FrameAllocator;
use crate::mm::fault::PageFaultHandler;
use crate::mm::page::Page;
use crate::register_mountable_fs;
use crate::{
    arch::mm::LockedFrameAllocator,
    arch::MMArch,
    driver::base::device::device_number::{DeviceNumber, Major},
    filesystem::vfs::{vcore::generate_inode_id, FileType},
    ipc::pipe::LockedPipeInode,
    libs::casting::DowncastArc,
    libs::mutex::{Mutex, MutexGuard},
    mm::MemoryManagementArch,
    process::ProcessManager,
    time::PosixTimeSpec,
};

use alloc::string::ToString;
use alloc::{
    collections::BTreeMap,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

use super::vfs::{
    file::FilePrivateData, mount::MountFlags, utils::DName, FileSystem, FsInfo,
    FsReconfigureRequest, IndexNode, InodeFlags, InodeId, InodeMode, Metadata, SpecialNodeData,
};

use linkme::distributed_slice;

use super::vfs::{Magic, MountableFileSystem, SuperBlock};
use lazy_static::lazy_static;

const TMPFS_MAX_NAMELEN: usize = 255;
const TMPFS_BLOCK_SIZE: u64 = 4096;

const TMPFS_DEFAULT_MIN_SIZE_BYTES: usize = 16 * 1024 * 1024; // 16MiB
const TMPFS_DEFAULT_MAX_SIZE_BYTES: usize = 4 * 1024 * 1024 * 1024; // 4GiB
const WHITEOUT_DEV: DeviceNumber = DeviceNumber::new(Major::UNNAMED_MAJOR, 0);

#[derive(Debug)]
struct TmpfsPageCacheBackend {
    inode: Weak<dyn IndexNode>,
    fs: Weak<Tmpfs>,
}

impl TmpfsPageCacheBackend {
    fn new(inode: Weak<dyn IndexNode>, fs: Weak<Tmpfs>) -> Self {
        Self { inode, fs }
    }
}

impl PageCacheBackend for TmpfsPageCacheBackend {
    fn read_page(&self, _index: usize, _buf: &mut [u8]) -> Result<usize, SystemError> {
        Ok(0)
    }

    fn write_page(&self, _index: usize, buf: &[u8]) -> Result<usize, SystemError> {
        Ok(buf.len())
    }

    fn npages(&self) -> usize {
        let inode = match self.inode.upgrade() {
            Some(inode) => inode,
            None => return 0,
        };
        match inode.metadata() {
            Ok(metadata) => {
                let size = metadata.size.max(0) as usize;
                if size == 0 {
                    0
                } else {
                    (size + MMArch::PAGE_SIZE - 1) >> MMArch::PAGE_SHIFT
                }
            }
            Err(_) => 0,
        }
    }

    fn reserve_page(&self) -> Result<(), SystemError> {
        self.fs
            .upgrade()
            .ok_or(SystemError::EIO)?
            .increase_size(MMArch::PAGE_SIZE as u64)
    }

    fn release_page(&self) {
        if let Some(fs) = self.fs.upgrade() {
            fs.decrease_size(MMArch::PAGE_SIZE);
        }
    }
}

fn tmpfs_move_entry_between_dirs(
    src_dir: &mut TmpfsInode,
    dst_dir: &mut TmpfsInode,
    old_key: &DName,
    new_key: &DName,
    flags: RenameFlags,
) -> Result<(), SystemError> {
    tmpfs_require_live_dir(src_dir)?;
    tmpfs_require_live_dir(dst_dir)?;

    let src_self = src_dir.self_ref.upgrade().ok_or(SystemError::EIO)?;
    let dst_self = dst_dir.self_ref.upgrade().ok_or(SystemError::EIO)?;

    let inode_to_move = src_dir
        .children
        .get(old_key)
        .cloned()
        .ok_or(SystemError::ENOENT)?;
    let old_type = inode_to_move.0.lock().metadata.file_type;

    if flags.contains(RenameFlags::EXCHANGE) {
        let existing = dst_dir
            .children
            .get(new_key)
            .cloned()
            .ok_or(SystemError::ENOENT)?;
        if Arc::ptr_eq(&inode_to_move, &existing) {
            return Ok(());
        }
        let now = PosixTimeSpec::now();
        let existing_type = existing.0.lock().metadata.file_type;

        src_dir.children.insert(old_key.clone(), existing.clone());
        dst_dir
            .children
            .insert(new_key.clone(), inode_to_move.clone());
        if old_type == FileType::Dir {
            src_dir.metadata.nlinks = src_dir.metadata.nlinks.saturating_sub(1);
            dst_dir.metadata.nlinks = dst_dir.metadata.nlinks.saturating_add(1);
        }
        if existing_type == FileType::Dir {
            dst_dir.metadata.nlinks = dst_dir.metadata.nlinks.saturating_sub(1);
            src_dir.metadata.nlinks = src_dir.metadata.nlinks.saturating_add(1);
        }

        {
            let mut moved = inode_to_move.0.lock();
            moved.parent = Arc::downgrade(&dst_self);
            moved.name = new_key.clone();
            moved.metadata.ctime = now;
        }
        {
            let mut replaced = existing.0.lock();
            replaced.parent = Arc::downgrade(&src_self);
            replaced.name = old_key.clone();
            replaced.metadata.ctime = now;
        }
        tmpfs_touch_dir(src_dir, now);
        tmpfs_touch_dir(dst_dir, now);
        return Ok(());
    }

    if let Some(existing) = dst_dir.children.get(new_key).cloned() {
        if flags.contains(RenameFlags::NOREPLACE) {
            return Err(SystemError::EEXIST);
        }

        // Avoid self-deadlock: `existing` may be `src_dir`/`dst_dir` itself.
        if Arc::ptr_eq(&existing, &src_self) {
            // Example: rename("dir/subdir", "dir") -> ENOTEMPTY (dir not empty).
            // Linux expects ENOTEMPTY for this case (TargetIsAncestorOfSource).
            return Err(SystemError::ENOTEMPTY);
        }
        if Arc::ptr_eq(&existing, &dst_self) {
            // Shouldn't happen in normal tmpfs (no self entry), but treat as busy.
            return Err(SystemError::EBUSY);
        }

        let (existing_id, existing_type, existing_dir_nonempty) = {
            let guard = existing.0.lock();
            let t = guard.metadata.file_type;
            let nonempty = t == FileType::Dir && !guard.children.is_empty();
            (guard.metadata.inode_id, t, nonempty)
        };

        let to_move_id = inode_to_move.0.lock().metadata.inode_id;
        if existing_id == to_move_id {
            // Destination already points to the same inode. For files this is
            // effectively removing the old entry.
            src_dir.children.remove(old_key);
            let now = PosixTimeSpec::now();
            tmpfs_touch_dir(src_dir, now);
            inode_to_move.0.lock().metadata.ctime = now;
            return Ok(());
        }

        if old_type == FileType::Dir && existing_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }
        if old_type != FileType::Dir && existing_type == FileType::Dir {
            return Err(SystemError::EISDIR);
        }
        if old_type == FileType::Dir && existing_dir_nonempty {
            return Err(SystemError::ENOTEMPTY);
        }

        // Remove existing destination entry (replacement).
        dst_dir.children.remove(new_key);
        let mut existing_guard = existing.0.lock();
        if existing_type == FileType::Dir {
            dst_dir.metadata.nlinks = dst_dir.metadata.nlinks.saturating_sub(1);
            existing_guard.metadata.nlinks = 0;
        } else {
            existing_guard.metadata.nlinks = existing_guard.metadata.nlinks.saturating_sub(1);
        }
        existing_guard.metadata.ctime = PosixTimeSpec::now();
    }

    // Remove from source directory.
    src_dir.children.remove(old_key);
    if flags.contains(RenameFlags::WHITEOUT) {
        tmpfs_insert_whiteout(src_dir, old_key)?;
    }
    if old_type == FileType::Dir {
        src_dir.metadata.nlinks = src_dir.metadata.nlinks.saturating_sub(1);
        dst_dir.metadata.nlinks = dst_dir.metadata.nlinks.saturating_add(1);
    }

    // Insert into destination directory and update inode bookkeeping.
    dst_dir
        .children
        .insert(new_key.clone(), inode_to_move.clone());
    let mut moved = inode_to_move.0.lock();
    moved.parent = Arc::downgrade(&dst_self);
    moved.name = new_key.clone();
    let now = PosixTimeSpec::now();
    moved.metadata.ctime = now;
    tmpfs_touch_dir(src_dir, now);
    tmpfs_touch_dir(dst_dir, now);

    Ok(())
}

fn tmpfs_touch_dir(dir: &mut TmpfsInode, now: PosixTimeSpec) {
    dir.metadata.mtime = now;
    dir.metadata.ctime = now;
}

fn tmpfs_require_live_dir(dir: &TmpfsInode) -> Result<(), SystemError> {
    if dir.metadata.file_type != FileType::Dir {
        return Err(SystemError::ENOTDIR);
    }
    if dir.metadata.nlinks == 0 {
        return Err(SystemError::ENOENT);
    }
    Ok(())
}

fn tmpfs_insert_whiteout(dir: &mut TmpfsInode, name: &DName) -> Result<(), SystemError> {
    if dir.children.contains_key(name) {
        return Err(SystemError::EEXIST);
    }

    let now = PosixTimeSpec::now();
    let whiteout = Arc::new(LockedTmpfsInode::new(TmpfsInode {
        parent: dir.self_ref.clone(),
        self_ref: Weak::default(),
        children: BTreeMap::new(),
        page_cache: None,
        metadata: Metadata {
            dev_id: 0,
            inode_id: generate_inode_id(),
            size: 0,
            blk_size: 0,
            blocks: 0,
            atime: now,
            mtime: now,
            ctime: now,
            btime: now,
            file_type: FileType::CharDevice,
            mode: InodeMode::S_IFCHR | InodeMode::from_bits_truncate(0o600),
            nlinks: 1,
            uid: 0,
            gid: 0,
            raw_dev: WHITEOUT_DEV,
            flags: InodeFlags::empty(),
        },
        fs: dir.fs.clone(),
        special_node: None,
        inline_symlink: None,
        name: name.clone(),
    }));
    whiteout.0.lock().self_ref = Arc::downgrade(&whiteout);
    dir.children.insert(name.clone(), whiteout);
    Ok(())
}

#[derive(Debug)]
pub struct LockedTmpfsInode(pub Mutex<TmpfsInode>, RwSem<()>);

impl LockedTmpfsInode {
    fn new(inode: TmpfsInode) -> Self {
        Self(Mutex::new(inode), RwSem::new(()))
    }
}

#[derive(Debug)]
pub struct Tmpfs {
    root_inode: Arc<LockedTmpfsInode>,
    super_block: RwSem<SuperBlock>,
    size_limit: RwSem<Option<u64>>,
    current_size: AtomicU64,
    mount_mode: RwSem<InodeMode>,
}

#[derive(Debug)]
pub struct TmpfsShmemFile {
    inode: Arc<dyn IndexNode>,
    fs: Arc<Tmpfs>,
    inode_id: InodeId,
    page_cache: Arc<PageCache>,
    charged_size: usize,
}

impl TmpfsShmemFile {
    pub fn inode(&self) -> Arc<dyn IndexNode> {
        self.inode.clone()
    }

    pub fn inode_id(&self) -> InodeId {
        self.inode_id
    }

    pub fn page_cache(&self) -> Arc<PageCache> {
        self.page_cache.clone()
    }

    pub fn set_locked(&self, locked: bool) -> (Arc<PageCache>, bool) {
        let page_cache = self.page_cache();
        let old_locked = page_cache.set_unevictable(locked);
        (page_cache, old_locked)
    }
}

impl Drop for TmpfsShmemFile {
    fn drop(&mut self) {
        self.fs.decrease_size(self.charged_size);
    }
}

#[derive(Debug)]
pub struct TmpfsInode {
    parent: Weak<LockedTmpfsInode>,
    self_ref: Weak<LockedTmpfsInode>,
    children: BTreeMap<DName, Arc<LockedTmpfsInode>>,
    page_cache: Option<Arc<PageCache>>,
    metadata: Metadata,
    fs: Weak<Tmpfs>,
    special_node: Option<SpecialNodeData>,
    inline_symlink: Option<String>,
    name: DName,
}

impl TmpfsInode {
    pub fn new() -> Self {
        Self {
            parent: Weak::default(),
            self_ref: Weak::default(),
            children: BTreeMap::new(),
            page_cache: None,
            metadata: Metadata {
                dev_id: 0,
                inode_id: generate_inode_id(),
                size: 0,
                blk_size: 0,
                blocks: 0,
                atime: PosixTimeSpec::default(),
                mtime: PosixTimeSpec::default(),
                ctime: PosixTimeSpec::default(),
                btime: PosixTimeSpec::default(),
                file_type: FileType::Dir,
                mode: InodeMode::S_IRWXUGO,
                nlinks: 2,
                uid: 0,
                gid: 0,
                raw_dev: DeviceNumber::default(),
                flags: InodeFlags::empty(),
            },
            fs: Weak::default(),
            special_node: None,
            inline_symlink: None,
            name: Default::default(),
        }
    }
}

#[derive(Debug)]
pub struct TmpfsMountData {
    mode: Option<InodeMode>,
    size_bytes: Option<u64>,
}

impl TmpfsMountData {
    fn parse(raw: Option<&str>) -> Result<Self, SystemError> {
        let mut mode = None;
        let mut size_bytes = None;

        if let Some(raw) = raw {
            for opt in raw.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
                if let Some(v) = opt.strip_prefix("mode=").map(|s| s.trim()) {
                    // mode 参数按八进制解析（mount 的习惯用法，如 755 = rwxr-xr-x）
                    let parsed = u32::from_str_radix(v, 8).map_err(|_| SystemError::EINVAL)?;
                    mode = Some(InodeMode::from_bits_truncate(parsed));
                } else if let Some(v) = opt.strip_prefix("size=").map(|s| s.trim()) {
                    // 支持大小写后缀：g/G, m/M, k/K
                    let v_lower = v.to_lowercase();
                    let (num_str, mul) = if let Some(s) = v_lower.strip_suffix('g') {
                        (s, 1u64 << 30)
                    } else if let Some(s) = v_lower.strip_suffix('m') {
                        (s, 1u64 << 20)
                    } else if let Some(s) = v_lower.strip_suffix('k') {
                        (s, 1u64 << 10)
                    } else {
                        (&v_lower[..], 1u64)
                    };
                    let base = num_str.parse::<u64>().map_err(|_| SystemError::EINVAL)?;
                    let bytes = base.checked_mul(mul).ok_or(SystemError::EINVAL)?;
                    let rounded = bytes
                        .checked_add(MMArch::PAGE_SIZE as u64 - 1)
                        .ok_or(SystemError::EINVAL)?
                        & !(MMArch::PAGE_SIZE as u64 - 1);
                    size_bytes = Some(rounded);
                } else {
                    return Err(SystemError::EINVAL);
                }
            }
        }

        Ok(Self { mode, size_bytes })
    }
}

impl FileSystemMakerData for TmpfsMountData {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl FileSystem for Tmpfs {
    fn supports_reliable_flush(&self) -> bool {
        // tmpfs has no crash-surviving backing state. A loop image stored here
        // disappears as a whole on power loss, so there is no partially
        // durable post-crash image for JBD2 to recover.
        true
    }

    unsafe fn fault(
        &self,
        pfm: &mut crate::mm::fault::PageFaultMessage,
    ) -> crate::mm::VmFaultReason {
        // tmpfs 是纯 page-cache 后端，不应走 pread/磁盘路径。
        PageFaultHandler::pagecache_fault_zero(pfm)
    }

    unsafe fn page_mkwrite(
        &self,
        pfm: &mut crate::mm::fault::PageFaultMessage,
    ) -> crate::mm::VmFaultReason {
        PageFaultHandler::filemap_page_mkwrite(pfm)
    }

    unsafe fn map_pages(
        &self,
        pfm: &mut crate::mm::fault::PageFaultMessage,
        start_pgoff: usize,
        end_pgoff: usize,
    ) -> crate::mm::VmFaultReason {
        PageFaultHandler::filemap_map_pages(pfm, start_pgoff, end_pgoff)
    }
    fn root_inode(&self) -> Arc<dyn super::vfs::IndexNode> {
        self.root_inode.clone()
    }

    fn info(&self) -> FsInfo {
        FsInfo {
            blk_dev_id: 0,
            max_name_len: TMPFS_MAX_NAMELEN,
        }
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        "tmpfs"
    }

    fn proc_show_mount_options(
        &self,
        _mount: &super::vfs::mount::MountFS,
        out: &mut dyn Write,
    ) -> Result<(), SystemError> {
        let mode = *self.mount_mode.read();
        if mode != InodeMode::S_IRWXUGO {
            write!(out, "mode={:03o}", mode.bits() & 0o7777).map_err(|_| SystemError::EINVAL)?;
        }
        Ok(())
    }

    fn super_block(&self) -> SuperBlock {
        let limit = self.size_limit.read();
        let mut sb = self.super_block.read().clone();
        if let Some(limit) = *limit {
            let current = self.current_size.load(Ordering::Acquire);
            let total_blocks = limit / TMPFS_BLOCK_SIZE;
            let used_blocks = Self::bytes_to_blocks_ceil(current);
            let free_blocks = total_blocks.saturating_sub(used_blocks);
            sb.blocks = total_blocks;
            sb.bfree = free_blocks;
            sb.bavail = free_blocks;
            sb.frsize = TMPFS_BLOCK_SIZE;
        }
        sb
    }

    fn support_readahead(&self) -> bool {
        // tmpfs 是内存文件系统，数据已经在 page_cache 中，不需要 readahead
        false
    }

    fn reconfigure(&self, request: FsReconfigureRequest<'_>) -> Result<MountFlags, SystemError> {
        let parsed = TmpfsMountData::parse(request.raw_data)?;

        if let Some(new_limit) = parsed.size_bytes {
            let mut limit = self.size_limit.write();
            let current = self.current_size.load(Ordering::Acquire);
            if new_limit < current {
                return Err(SystemError::EINVAL);
            }
            *limit = Some(new_limit);
        }

        if let Some(mode) = parsed.mode {
            let mut root = self.root_inode.0.lock();
            root.metadata.mode = mode;
            *self.mount_mode.write() = mode;
        }

        Ok(request.sb_flags & request.sb_flags_mask)
    }
}

impl Tmpfs {
    #[inline]
    fn default_size_bytes() -> usize {
        // 与 /proc/meminfo 一致：从帧分配器获取物理内存总量。
        let total = unsafe { LockedFrameAllocator.usage() }.total().bytes();
        let half = total / 2;
        half.clamp(TMPFS_DEFAULT_MIN_SIZE_BYTES, TMPFS_DEFAULT_MAX_SIZE_BYTES)
    }

    #[inline]
    fn bytes_to_blocks_ceil(bytes: u64) -> u64 {
        bytes.div_ceil(TMPFS_BLOCK_SIZE)
    }

    pub fn new(mount_data: &TmpfsMountData) -> Arc<Self> {
        // 若未指定 size=，使用默认容量策略（通常为物理内存的一半）。
        // 这样 busybox df -h（默认过滤 f_blocks==0）就能显示 /tmp。
        let size_limit = mount_data
            .size_bytes
            .or_else(|| Some(Self::default_size_bytes() as u64));
        Self::new_with_size_limit(mount_data.mode, size_limit)
    }

    fn new_with_size_limit(mode: Option<InodeMode>, size_limit: Option<u64>) -> Arc<Self> {
        let mut sb = SuperBlock::new(
            Magic::TMPFS_MAGIC,
            TMPFS_BLOCK_SIZE,
            TMPFS_MAX_NAMELEN as u64,
        );
        sb.frsize = TMPFS_BLOCK_SIZE;
        if let Some(size) = size_limit {
            let blocks = size / TMPFS_BLOCK_SIZE;
            sb.blocks = blocks;
            sb.bfree = blocks;
            sb.bavail = blocks;
        }

        let root: Arc<LockedTmpfsInode> = Arc::new(LockedTmpfsInode::new(TmpfsInode::new()));

        let result: Arc<Tmpfs> = Arc::new(Tmpfs {
            root_inode: root,
            super_block: RwSem::new(sb),
            size_limit: RwSem::new(size_limit),
            current_size: AtomicU64::new(0),
            mount_mode: RwSem::new(mode.unwrap_or(InodeMode::S_IRWXUGO)),
        });

        let mut root_guard: MutexGuard<TmpfsInode> = result.root_inode.0.lock();
        root_guard.parent = Arc::downgrade(&result.root_inode);
        root_guard.self_ref = Arc::downgrade(&result.root_inode);
        root_guard.fs = Arc::downgrade(&result);
        root_guard.metadata.mode = mode.unwrap_or(InodeMode::S_IRWXUGO);
        drop(root_guard);

        result
    }

    pub fn new_unlimited(mode: Option<InodeMode>) -> Arc<Self> {
        Self::new_with_size_limit(mode, None)
    }

    /// 原子地增加文件系统使用的大小
    /// 返回Ok(())如果更新成功，Err(SystemError::ENOSPC)如果超过限制
    /// 使用compare_exchange_weak循环确保并发安全
    fn increase_size(&self, size_diff: u64) -> Result<(), SystemError> {
        let size_limit = self.size_limit.read();
        if let Some(limit) = *size_limit {
            // 使用compare_exchange_weak循环确保原子性
            loop {
                let current = self.current_size.load(Ordering::Acquire);
                let new_total = current.saturating_add(size_diff);

                if new_total > limit {
                    return Err(SystemError::ENOSPC);
                }

                // 原子地更新，如果current没有被其他线程修改，则更新成功
                match self.current_size.compare_exchange_weak(
                    current,
                    new_total,
                    Ordering::Release,
                    Ordering::Acquire,
                ) {
                    Ok(_) => break,     // 更新成功
                    Err(_) => continue, // 被其他线程修改，重试
                }
            }
        }
        Ok(())
    }

    /// 原子地减少文件系统当前使用的大小（用于文件删除或缩小）
    /// 使用fetch_sub确保并发安全
    fn decrease_size(&self, size: usize) {
        let size_limit = self.size_limit.read();
        if size_limit.is_some() {
            let size_to_decrease = size as u64;
            loop {
                let current = self.current_size.load(Ordering::Acquire);
                let new = current.saturating_sub(size_to_decrease);
                if self
                    .current_size
                    .compare_exchange_weak(current, new, Ordering::Release, Ordering::Acquire)
                    .is_ok()
                {
                    break;
                }
            }
        }
    }

    fn available_pages(&self) -> Option<usize> {
        let size_limit = self.size_limit.read();
        (*size_limit).map(|limit| {
            let current = self.current_size.load(Ordering::Acquire);
            (limit.saturating_sub(current) / MMArch::PAGE_SIZE as u64) as usize
        })
    }

    fn create_unlinked_shmem_inode(
        self: &Arc<Self>,
        name: DName,
        mode: InodeMode,
        size: usize,
    ) -> Result<Arc<TmpfsShmemFile>, SystemError> {
        if size > i64::MAX as usize {
            return Err(SystemError::EOVERFLOW);
        }
        let charged_size = size
            .checked_add(MMArch::PAGE_SIZE - 1)
            .ok_or(SystemError::EOVERFLOW)?
            & !(MMArch::PAGE_SIZE - 1);
        let charged_size_u64 = charged_size as u64;
        let blocks_u64 = Self::bytes_to_blocks_ceil(size as u64);
        if blocks_u64 > usize::MAX as u64 {
            return Err(SystemError::EOVERFLOW);
        }
        self.increase_size(charged_size_u64)?;

        let inode_id = generate_inode_id();
        let result: Arc<LockedTmpfsInode> = Arc::new(LockedTmpfsInode::new(TmpfsInode {
            parent: Weak::default(),
            self_ref: Weak::default(),
            children: BTreeMap::new(),
            page_cache: None,
            metadata: Metadata {
                dev_id: 0,
                inode_id,
                size: size as i64,
                blk_size: TMPFS_BLOCK_SIZE as usize,
                blocks: blocks_u64 as usize,
                atime: PosixTimeSpec::default(),
                mtime: PosixTimeSpec::default(),
                ctime: PosixTimeSpec::default(),
                btime: PosixTimeSpec::default(),
                file_type: FileType::File,
                mode,
                flags: InodeFlags::empty(),
                nlinks: 0,
                uid: 0,
                gid: 0,
                raw_dev: DeviceNumber::default(),
            },
            fs: Arc::downgrade(self),
            special_node: None,
            inline_symlink: None,
            name,
        }));

        result.0.lock().self_ref = Arc::downgrade(&result);
        let inode_dyn: Arc<dyn IndexNode> = result.clone();
        let backend = Arc::new(TmpfsPageCacheBackend::new(
            Arc::downgrade(&inode_dyn),
            Arc::downgrade(self),
        ));
        let pc = PageCache::new_shmem(Some(Arc::downgrade(&inode_dyn)), Some(backend));
        result.0.lock().page_cache = Some(pc.clone());

        Ok(Arc::new(TmpfsShmemFile {
            inode: inode_dyn,
            fs: self.clone(),
            inode_id,
            page_cache: pc,
            charged_size,
        }))
    }
}

lazy_static! {
    static ref SYSV_SHMEM_TMPFS: Arc<Tmpfs> = Tmpfs::new_unlimited(Some(InodeMode::S_IRWXUGO));
}

pub fn create_unlinked_shmem_file(size: usize) -> Result<Arc<TmpfsShmemFile>, SystemError> {
    static NEXT_SYSV_SHMEM_NAME: AtomicU64 = AtomicU64::new(1);
    let name = format!(
        "SYSV{:08x}",
        NEXT_SYSV_SHMEM_NAME.fetch_add(1, Ordering::Relaxed)
    );
    let name = DName::from(name.as_str());
    SYSV_SHMEM_TMPFS.create_unlinked_shmem_inode(
        name,
        InodeMode::S_IRUSR | InodeMode::S_IWUSR,
        size,
    )
}

impl MountableFileSystem for Tmpfs {
    fn make_mount_data(
        raw_data: Option<&str>,
        _source: &str,
    ) -> Result<Option<Arc<dyn FileSystemMakerData + 'static>>, SystemError> {
        let parsed = TmpfsMountData::parse(raw_data)?;
        Ok(Some(Arc::new(parsed)))
    }

    fn make_fs(
        data: Option<&dyn FileSystemMakerData>,
    ) -> Result<Arc<dyn FileSystem + 'static>, SystemError> {
        let d = data
            .ok_or(SystemError::EINVAL)?
            .as_any()
            .downcast_ref::<TmpfsMountData>()
            .ok_or(SystemError::EINVAL)?;
        Ok(Tmpfs::new(d))
    }
}

register_mountable_fs!(Tmpfs, TMPFSMAKER, "tmpfs");

impl IndexNode for LockedTmpfsInode {
    fn append_lock_fs(&self) -> Option<Arc<dyn FileSystem>> {
        Some(self.fs())
    }

    fn supports_post_write_sync(&self, file_type: FileType) -> bool {
        file_type == FileType::File
    }

    fn mmap(&self, _start: usize, _len: usize, _offset: usize) -> Result<(), SystemError> {
        Ok(())
    }

    fn truncate(&self, len: usize) -> Result<(), SystemError> {
        let inode = self.0.lock();
        if inode.metadata.file_type == FileType::Dir {
            return Err(SystemError::EINVAL);
        }
        drop(inode);
        // 复用 resize，保证扩展/收缩两侧逻辑一致
        self.resize(len)
    }

    fn close(&self, _data: MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        Ok(())
    }

    fn sync_file(
        &self,
        datasync: bool,
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<(), SystemError> {
        match self.metadata()?.file_type {
            FileType::File | FileType::Dir => {
                if datasync {
                    self.datasync()
                } else {
                    self.sync()
                }
            }
            _ => Err(SystemError::EINVAL),
        }
    }

    fn sync_file_range(
        &self,
        start: usize,
        end: usize,
        _datasync: bool,
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<(), SystemError> {
        match self.metadata()?.file_type {
            FileType::File | FileType::Dir => {
                if let Some(page_cache) = self.page_cache() {
                    let start_index = start >> MMArch::PAGE_SHIFT;
                    let end_index = end >> MMArch::PAGE_SHIFT;
                    page_cache
                        .manager()
                        .writeback_range(start_index, end_index)?;
                }
                Ok(())
            }
            _ => Err(SystemError::EINVAL),
        }
    }

    fn open(
        &self,
        _data: MutexGuard<FilePrivateData>,
        _mode: &super::vfs::file::FileFlags,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        if buf.len() < len {
            return Err(SystemError::EINVAL);
        }
        let inode = self.0.lock();
        if inode.metadata.file_type == FileType::Dir {
            return Err(SystemError::EISDIR);
        }
        let file_size = inode.metadata.size as usize;
        if let Some(target) = inode.inline_symlink.clone() {
            drop(inode);
            let read_len = if offset < target.len() {
                core::cmp::min(target.len() - offset, len)
            } else {
                0
            };
            if read_len > 0 {
                buf[..read_len].copy_from_slice(&target.as_bytes()[offset..offset + read_len]);
            }
            return Ok(read_len);
        }
        let page_cache = inode.page_cache.clone().ok_or(SystemError::EIO)?;
        drop(inode);

        // 计算实际读取长度
        let read_len = if offset < file_size {
            core::cmp::min(file_size - offset, len)
        } else {
            0
        };

        if read_len == 0 {
            return Ok(0);
        }

        let start_page_index = offset >> MMArch::PAGE_SHIFT;
        let end_page_index = (offset + read_len - 1) >> MMArch::PAGE_SHIFT;
        // 两阶段读取：
        // 1) 持有 page_cache 锁：只做“取页/建页 + 收集引用”，绝不触碰用户缓冲区
        // 2) 释放 page_cache 锁：再把页内容拷贝到用户缓冲区（并做 prefault）
        struct ReadItem {
            page: Option<Arc<Page>>,
            page_offset: usize,
            sub_len: usize,
        }

        let mut items: Vec<ReadItem> = Vec::new();
        for page_index in start_page_index..=end_page_index {
            let page_start = page_index * MMArch::PAGE_SIZE;
            let page_end = page_start + MMArch::PAGE_SIZE;

            let read_start = core::cmp::max(offset, page_start);
            let read_end = core::cmp::min(offset + read_len, page_end);
            let page_read_len = read_end.saturating_sub(read_start);
            if page_read_len == 0 {
                continue;
            }

            // Reading a sparse tmpfs hole returns zeroes without allocating a
            // page or consuming the mount's block quota.
            let page = page_cache.manager().peek_page(page_index);

            items.push(ReadItem {
                page,
                page_offset: read_start - page_start,
                sub_len: page_read_len,
            });
        }

        let mut dst_off = 0usize;
        for it in items {
            if it.sub_len == 0 {
                continue;
            }

            // prefault：避免在任何锁持有期间缺页（SelfRead 的关键）
            let v = volatile_read!(buf[dst_off]);
            volatile_write!(buf[dst_off], v);
            let v = volatile_read!(buf[dst_off + it.sub_len - 1]);
            volatile_write!(buf[dst_off + it.sub_len - 1], v);

            if let Some(page) = it.page {
                let page_guard = page.read();
                unsafe {
                    buf[dst_off..dst_off + it.sub_len].copy_from_slice(
                        &page_guard.as_slice()[it.page_offset..it.page_offset + it.sub_len],
                    );
                }
            } else {
                buf[dst_off..dst_off + it.sub_len].fill(0);
            }
            dst_off += it.sub_len;
        }

        Ok(read_len)
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        if buf.len() < len {
            return Err(SystemError::EINVAL);
        }

        // Linux 语义：写入 0 字节应当成功返回 0，且不改变文件偏移/大小。
        // 同时避免后续 (offset + len - 1) 的下溢导致超大页范围遍历。
        if len == 0 {
            return Ok(0);
        }
        let _size_guard = self.1.read();
        let inode = self.0.lock();
        if inode.metadata.file_type == FileType::Dir {
            return Err(SystemError::EISDIR);
        }
        let page_cache = inode.page_cache.clone().ok_or(SystemError::EIO)?;
        let write_end = offset.checked_add(len).ok_or(SystemError::EFBIG)?;
        drop(inode);

        let start_page_index = offset >> MMArch::PAGE_SHIFT;
        let end_page_index = (write_end - 1) >> MMArch::PAGE_SHIFT;
        let mut written = 0usize;
        for page_index in start_page_index..=end_page_index {
            let page_start = page_index * MMArch::PAGE_SIZE;
            let page_end = page_start + MMArch::PAGE_SIZE;

            let write_start = core::cmp::max(offset, page_start);
            let page_write_end = core::cmp::min(write_end, page_end);
            let page_write_len = page_write_end.saturating_sub(write_start);
            if page_write_len == 0 {
                continue;
            }

            let pin = match page_cache.manager().commit_overwrite_pinned(page_index) {
                Ok(pin) => pin,
                Err(err) => {
                    if written == 0 {
                        return Err(err);
                    }
                    break;
                }
            };

            // prefault 用户缓冲区，避免后续在持页锁时缺页
            volatile_read!(buf[written]);
            volatile_read!(buf[written + page_write_len - 1]);

            let page = pin.page();
            let mut page_guard = page.write();
            unsafe {
                let page_offset = write_start - page_start;
                page_guard.as_slice_mut()[page_offset..page_offset + page_write_len]
                    .copy_from_slice(&buf[written..written + page_write_len]);
            }
            page_guard.add_flags(crate::mm::page::PageFlags::PG_DIRTY);
            drop(page_guard);
            if let Err(err) = page_cache.manager().update_page(page_index) {
                if written == 0 {
                    return Err(err);
                }
                break;
            }
            written += page_write_len;
        }

        // Quota is charged by page-cache membership. Logical size advances
        // only through the prefix which was actually copied.
        let mut inode = self.0.lock();
        let committed_end = offset + written;
        if committed_end > inode.metadata.size as usize {
            inode.metadata.size = committed_end as i64;
        }
        Ok(written)
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        self.0.lock().fs.upgrade().unwrap()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        let inode = self.0.lock();
        Ok(inode.metadata.clone())
    }

    fn set_metadata(&self, metadata: &Metadata) -> Result<(), SystemError> {
        let mut inode = self.0.lock();
        inode.metadata.atime = metadata.atime;
        inode.metadata.mtime = metadata.mtime;
        inode.metadata.ctime = metadata.ctime;
        inode.metadata.btime = metadata.btime;
        inode.metadata.mode = metadata.mode;
        inode.metadata.uid = metadata.uid;
        inode.metadata.gid = metadata.gid;
        Ok(())
    }

    fn update_atime(&self, now: PosixTimeSpec, relatime: bool) -> Result<(), SystemError> {
        let mut inode = self.0.lock();
        crate::filesystem::vfs::update_atime_locked(&mut inode.metadata, now, relatime);
        Ok(())
    }

    fn resize(&self, len: usize) -> Result<(), SystemError> {
        let _size_guard = self.1.write();
        let (old_size, new_size, page_cache) = {
            let mut inode = self.0.lock();
            if inode.metadata.file_type != FileType::File {
                return Err(SystemError::EINVAL);
            }

            let old_size = inode.metadata.size as usize;
            let new_size = len;

            // Linux truncate_setsize() writes the new i_size before truncating page cache.
            // Drop the inode lock before page-cache unmap/truncate so page faults do not
            // form an inode-lock/MM-lock ABBA with the truncate path.
            inode.metadata.size = len as i64;
            (old_size, new_size, inode.page_cache.clone())
        };

        if new_size < old_size {
            if let Some(pc) = page_cache {
                pc.manager().resize(len)?;
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
        if mode != 0 {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        if len == 0 {
            return Err(SystemError::EINVAL);
        }
        let end = offset.checked_add(len).ok_or(SystemError::EFBIG)?;
        if end > isize::MAX as usize {
            return Err(SystemError::EFBIG);
        }
        crate::filesystem::vfs::vcore::check_file_size_limit(end)?;

        let _size_guard = self.1.write();
        let (page_cache, fs) = {
            let inode = self.0.lock();
            (
                inode.page_cache.clone().ok_or(SystemError::EIO)?,
                inode.fs.upgrade().ok_or(SystemError::EIO)?,
            )
        };
        let first = offset >> MMArch::PAGE_SHIFT;
        let last = (end - 1) >> MMArch::PAGE_SHIFT;
        let mut created: Vec<(usize, Arc<Page>)> = Vec::new();
        let missing_pages = page_cache.manager().missing_pages_in_range(first, last)?;
        if fs
            .available_pages()
            .is_some_and(|available| missing_pages > available)
        {
            return Err(SystemError::ENOSPC);
        }
        created
            .try_reserve_exact(missing_pages)
            .map_err(|_| SystemError::ENOMEM)?;

        for page_index in first..=last {
            match page_cache
                .manager()
                .commit_overwrite_pinned_with_status(page_index)
            {
                Ok((pin, was_created)) => {
                    if was_created {
                        if created.len() == created.capacity() && created.try_reserve(1).is_err() {
                            let current_page = pin.page();
                            drop(pin);
                            let _ = page_cache
                                .manager()
                                .discard_created_page(page_index, &current_page);
                            for (created_index, created_page) in created.into_iter().rev() {
                                let _ = page_cache
                                    .manager()
                                    .discard_created_page(created_index, &created_page);
                            }
                            return Err(SystemError::ENOMEM);
                        }
                        created.push((page_index, pin.page()));
                    }
                }
                Err(error) => {
                    for (created_index, created_page) in created.into_iter().rev() {
                        let _ = page_cache
                            .manager()
                            .discard_created_page(created_index, &created_page);
                    }
                    return Err(error);
                }
            }
        }

        // Match Linux shmem_fallocate(): only after every page has been
        // allocated successfully, apply the same write-side metadata effects
        // as a regular file modification.  Build the update from the latest
        // metadata while holding the inode lock so concurrent chmod/chown
        // changes cannot be overwritten by a stale pre-allocation snapshot.
        let cred = ProcessManager::current_pcb().cred();
        let mut inode = self.0.lock();
        let new_size = core::cmp::max(inode.metadata.size.max(0) as usize, end);
        let (metadata, mask) =
            crate::filesystem::vfs::vcore::prepare_write_side_effect_metadata_with_cred(
                inode.metadata.clone(),
                new_size,
                &cred,
            );
        inode.metadata.size = metadata.size;
        crate::filesystem::vfs::merge_metadata_masked(&mut inode.metadata, &metadata, mask);
        let _ = lock_owner;
        Ok(())
    }

    fn symlink(&self, name: &str, target: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        const SHORT_SYMLINK_LEN: usize = 128;

        if target
            .len()
            .checked_add(1)
            .ok_or(SystemError::ENAMETOOLONG)?
            > MMArch::PAGE_SIZE
        {
            return Err(SystemError::ENAMETOOLONG);
        }

        let name = DName::from(name);
        let mut parent = self.0.lock();
        tmpfs_require_live_dir(&parent)?;
        if parent.children.contains_key(&name) {
            return Err(SystemError::EEXIST);
        }
        // Revalidate local DAC while holding the same lock that protects the
        // parent metadata and publishes the child. This is the tmpfs analogue
        // of Linux holding the parent inode lock across may_create()+symlink.
        if parent.metadata.flags.contains(InodeFlags::S_IMMUTABLE) {
            return Err(SystemError::EPERM);
        }
        if ProcessManager::initialized() {
            let cred = ProcessManager::current_pcb().cred();
            cred.inode_permission(
                &parent.metadata,
                (crate::filesystem::vfs::permission::PermissionMask::MAY_WRITE
                    | crate::filesystem::vfs::permission::PermissionMask::MAY_EXEC)
                    .bits(),
            )?;
        }
        let init = crate::filesystem::vfs::permission::child_inode_init(
            &parent.metadata,
            FileType::SymLink,
            InodeMode::S_IRWXUGO,
        );

        let now = PosixTimeSpec::now();
        let inline = target.len() < SHORT_SYMLINK_LEN;
        let result = Arc::new(LockedTmpfsInode::new(TmpfsInode {
            parent: parent.self_ref.clone(),
            self_ref: Weak::default(),
            children: BTreeMap::new(),
            page_cache: None,
            metadata: Metadata {
                dev_id: 0,
                inode_id: generate_inode_id(),
                size: target.len() as i64,
                blk_size: TMPFS_BLOCK_SIZE as usize,
                blocks: if inline { 0 } else { 1 },
                atime: now,
                mtime: now,
                ctime: now,
                btime: now,
                file_type: FileType::SymLink,
                mode: init.mode,
                flags: InodeFlags::empty(),
                nlinks: 1,
                uid: init.uid,
                gid: init.gid,
                raw_dev: DeviceNumber::default(),
            },
            fs: parent.fs.clone(),
            special_node: None,
            inline_symlink: inline.then(|| target.to_string()),
            name: name.clone(),
        }));
        result.0.lock().self_ref = Arc::downgrade(&result);

        if !inline {
            let inode_dyn: Arc<dyn IndexNode> = result.clone();
            let backend = Arc::new(TmpfsPageCacheBackend::new(
                Arc::downgrade(&inode_dyn),
                parent.fs.clone(),
            ));
            let page_cache = PageCache::new_shmem(Some(Arc::downgrade(&inode_dyn)), Some(backend));
            result.0.lock().page_cache = Some(page_cache.clone());
            let page = page_cache.manager().commit_overwrite(0)?;
            let mut page_guard = page.write();
            unsafe {
                page_guard.as_slice_mut()[..target.len()].copy_from_slice(target.as_bytes());
            }
            page_guard.add_flags(crate::mm::page::PageFlags::PG_DIRTY);
            drop(page_guard);
            page_cache.manager().update_page(0)?;
        }

        parent.children.insert(name, result.clone());
        tmpfs_touch_dir(&mut parent, now);
        Ok(result)
    }

    fn create_with_data(
        &self,
        name: &str,
        file_type: FileType,
        mode: InodeMode,
        data: usize,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let name = DName::from(name);
        let mut inode = self.0.lock();
        tmpfs_require_live_dir(&inode)?;
        if inode.children.contains_key(&name) {
            return Err(SystemError::EEXIST);
        }
        let init =
            crate::filesystem::vfs::permission::child_inode_init(&inode.metadata, file_type, mode);

        let now = PosixTimeSpec::now();
        let result: Arc<LockedTmpfsInode> = Arc::new(LockedTmpfsInode::new(TmpfsInode {
            parent: inode.self_ref.clone(),
            self_ref: Weak::default(),
            children: BTreeMap::new(),
            page_cache: None,
            metadata: Metadata {
                dev_id: 0,
                inode_id: generate_inode_id(),
                size: 0,
                blk_size: 0,
                blocks: 0,
                atime: now,
                mtime: now,
                ctime: now,
                btime: now,
                file_type,
                mode: init.mode,
                flags: InodeFlags::empty(),
                nlinks: if file_type == FileType::Dir { 2 } else { 1 },
                uid: init.uid,
                gid: init.gid,
                raw_dev: DeviceNumber::from(data as u32),
            },
            fs: inode.fs.clone(),
            special_node: None,
            inline_symlink: None,
            name: name.clone(),
        }));

        result.0.lock().self_ref = Arc::downgrade(&result);

        // tmpfs 中：普通文件和符号链接都需要可读写的数据存储。
        // 目前 VFS 使用 read_at/write_at 来读写 symlink 内容（readlink/symlink 语义），
        // 因此 symlink 也必须有 page_cache 后端，否则会在 write_at/read_at 返回 EIO。
        if file_type == FileType::File || file_type == FileType::SymLink {
            let backend = Arc::new(TmpfsPageCacheBackend::new(
                Arc::downgrade(&result) as Weak<dyn IndexNode>,
                inode.fs.clone(),
            ));
            let pc = PageCache::new_shmem(
                Some(Arc::downgrade(&result) as Weak<dyn IndexNode>),
                Some(backend),
            );
            result.0.lock().page_cache = Some(pc);
        }

        inode.children.insert(name, result.clone());
        if file_type == FileType::Dir {
            inode.metadata.nlinks += 1;
        }
        tmpfs_touch_dir(&mut inode, now);
        Ok(result)
    }

    fn link(&self, name: &str, other: &Arc<dyn IndexNode>) -> Result<(), SystemError> {
        // downcast 用于获取类型特定功能（跨文件系统检查已在 VFS 层完成）
        let other: &LockedTmpfsInode = other
            .downcast_ref::<LockedTmpfsInode>()
            .ok_or(SystemError::EINVAL)?;
        let name = DName::from(name);
        let mut inode: MutexGuard<TmpfsInode> = self.0.lock();
        let mut other_locked: MutexGuard<TmpfsInode> = other.0.lock();

        tmpfs_require_live_dir(&inode)?;
        if other_locked.metadata.file_type == FileType::Dir {
            return Err(SystemError::EISDIR);
        }
        if inode.children.contains_key(&name) {
            return Err(SystemError::EEXIST);
        }

        inode
            .children
            .insert(name, other_locked.self_ref.upgrade().unwrap());
        other_locked.metadata.nlinks += 1;
        let now = PosixTimeSpec::now();
        other_locked.metadata.ctime = now;
        tmpfs_touch_dir(&mut inode, now);
        Ok(())
    }

    fn unlink(&self, name: &str) -> Result<(), SystemError> {
        let mut inode: MutexGuard<TmpfsInode> = self.0.lock();
        tmpfs_require_live_dir(&inode)?;
        if name == "." || name == ".." {
            return Err(SystemError::ENOTEMPTY);
        }

        let name = DName::from(name);
        let to_delete = inode.children.get(&name).ok_or(SystemError::ENOENT)?;
        let deleted_inode = to_delete.0.lock();
        if deleted_inode.metadata.file_type == FileType::Dir {
            return Err(SystemError::EPERM);
        }

        drop(deleted_inode);

        let mut deleted_guard = to_delete.0.lock();
        deleted_guard.metadata.nlinks = deleted_guard
            .metadata
            .nlinks
            .checked_sub(1)
            .expect("tempfs nlinks underflow: filesystem corruption detected");

        let now = PosixTimeSpec::now();
        deleted_guard.metadata.ctime = now;
        drop(deleted_guard);

        inode.children.remove(&name);
        tmpfs_touch_dir(&mut inode, now);

        Ok(())
    }

    fn rmdir(&self, name: &str) -> Result<(), SystemError> {
        // 检查是否为 "." 或 ".."
        if name == "." {
            return Err(SystemError::EINVAL);
        }
        if name == ".." {
            return Err(SystemError::ENOTEMPTY);
        }

        let name = DName::from(name);
        let mut inode: MutexGuard<TmpfsInode> = self.0.lock();
        tmpfs_require_live_dir(&inode)?;
        let to_delete = inode.children.get(&name).ok_or(SystemError::ENOENT)?;
        let deleted_inode = to_delete.0.lock();
        if deleted_inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        // 检查目录是否为空（排除 "." 和 ".."）
        if !deleted_inode.children.is_empty() {
            return Err(SystemError::ENOTEMPTY);
        }

        drop(deleted_inode);
        let now = PosixTimeSpec::now();
        let mut deleted_inode = to_delete.0.lock();
        deleted_inode.metadata.nlinks = 0;
        deleted_inode.metadata.ctime = now;
        drop(deleted_inode);
        inode.children.remove(&name);
        inode.metadata.nlinks -= 1;
        tmpfs_touch_dir(&mut inode, now);

        Ok(())
    }

    fn move_to(
        &self,
        old_name: &str,
        target: &Arc<dyn IndexNode>,
        new_name: &str,
        flags: RenameFlags,
    ) -> Result<(), SystemError> {
        // tmpfs rename should move a directory entry (dentry move), not create
        // a hardlink+unlink pair. The latter breaks directory moves (unlink()
        // rejects directories) and can also lead to incorrect link/size accounting.

        let old_key = DName::from(old_name);
        let new_key = DName::from(new_name);

        // Target must be a directory in tmpfs.
        let target_locked = target
            .clone()
            .downcast_arc::<LockedTmpfsInode>()
            .ok_or(SystemError::EINVAL)?;

        // Lock ordering: lock by inode_id to avoid deadlocks.
        let self_id = self.0.lock().metadata.inode_id;
        let target_id = target_locked.0.lock().metadata.inode_id;

        if self_id == target_id {
            // Same directory rename.
            let mut dir = self.0.lock();
            tmpfs_require_live_dir(&dir)?;
            let inode_to_move = dir
                .children
                .get(&old_key)
                .cloned()
                .ok_or(SystemError::ENOENT)?;
            let old_type = inode_to_move.0.lock().metadata.file_type;

            if flags.contains(RenameFlags::EXCHANGE) {
                let existing = dir
                    .children
                    .get(&new_key)
                    .cloned()
                    .ok_or(SystemError::ENOENT)?;
                let to_move_id = inode_to_move.0.lock().metadata.inode_id;
                let existing_id = existing.0.lock().metadata.inode_id;
                if existing_id == to_move_id {
                    return Ok(());
                }

                let now = PosixTimeSpec::now();
                dir.children.insert(old_key.clone(), existing.clone());
                dir.children.insert(new_key.clone(), inode_to_move.clone());
                let mut existing = existing.0.lock();
                existing.name = old_key;
                existing.metadata.ctime = now;
                let mut moved = inode_to_move.0.lock();
                moved.name = new_key;
                moved.metadata.ctime = now;
                tmpfs_touch_dir(&mut dir, now);
                return Ok(());
            }

            if let Some(existing) = dir.children.get(&new_key).cloned() {
                if flags.contains(RenameFlags::NOREPLACE) {
                    return Err(SystemError::EEXIST);
                }

                // If destination already refers to the same inode, it's a no-op.
                let existing_id = existing.0.lock().metadata.inode_id;
                let to_move_id = inode_to_move.0.lock().metadata.inode_id;
                if existing_id == to_move_id {
                    return Ok(());
                }

                let existing_type = existing.0.lock().metadata.file_type;
                if old_type == FileType::Dir && existing_type != FileType::Dir {
                    return Err(SystemError::ENOTDIR);
                }
                if old_type != FileType::Dir && existing_type == FileType::Dir {
                    return Err(SystemError::EISDIR);
                }

                if old_type == FileType::Dir && !existing.0.lock().children.is_empty() {
                    return Err(SystemError::ENOTEMPTY);
                }

                // Remove existing destination entry (replacement).
                dir.children.remove(&new_key);
                let mut existing_guard = existing.0.lock();
                if existing_type == FileType::Dir {
                    dir.metadata.nlinks = dir.metadata.nlinks.saturating_sub(1);
                    existing_guard.metadata.nlinks = 0;
                } else {
                    existing_guard.metadata.nlinks =
                        existing_guard.metadata.nlinks.saturating_sub(1);
                }
                existing_guard.metadata.ctime = PosixTimeSpec::now();
            }

            // Move entry within the same directory.
            dir.children.remove(&old_key);
            if flags.contains(RenameFlags::WHITEOUT) {
                tmpfs_insert_whiteout(&mut dir, &old_key)?;
            }
            dir.children.insert(new_key.clone(), inode_to_move.clone());
            let now = PosixTimeSpec::now();
            let mut moved = inode_to_move.0.lock();
            moved.name = new_key;
            moved.metadata.ctime = now;
            tmpfs_touch_dir(&mut dir, now);
            return Ok(());
        }

        // Cross-directory move.
        // Lock both directories in a stable order.
        if self_id < target_id {
            let mut src_dir = self.0.lock();
            let mut dst_dir = target_locked.0.lock();
            return tmpfs_move_entry_between_dirs(
                &mut src_dir,
                &mut dst_dir,
                &old_key,
                &new_key,
                flags,
            );
        } else {
            let mut dst_dir = target_locked.0.lock();
            let mut src_dir = self.0.lock();
            return tmpfs_move_entry_between_dirs(
                &mut src_dir,
                &mut dst_dir,
                &old_key,
                &new_key,
                flags,
            );
        }
    }

    fn find(&self, name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        let inode = self.0.lock();

        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        match name {
            "" | "." => Ok(inode.self_ref.upgrade().ok_or(SystemError::ENOENT)?),
            ".." => Ok(inode.parent.upgrade().ok_or(SystemError::ENOENT)?),
            name => {
                let name = DName::from(name);
                Ok(inode
                    .children
                    .get(&name)
                    .ok_or(SystemError::ENOENT)?
                    .clone())
            }
        }
    }

    fn get_entry_name(&self, ino: InodeId) -> Result<String, SystemError> {
        let inode: MutexGuard<TmpfsInode> = self.0.lock();
        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        match ino.into() {
            0 => Ok(String::from(".")),
            1 => Ok(String::from("..")),
            ino => {
                let mut key: Vec<String> = inode
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
                    0 => Err(SystemError::ENOENT),
                    1 => Ok(key.remove(0)),
                    _ => Err(SystemError::EIO),
                }
            }
        }
    }

    fn list(&self) -> Result<Vec<String>, SystemError> {
        let info = self.metadata()?;
        if info.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        let mut keys: Vec<String> = Vec::new();
        keys.push(String::from("."));
        keys.push(String::from(".."));
        keys.append(
            &mut self
                .0
                .lock()
                .children
                .keys()
                .map(|k| k.to_string())
                .collect(),
        );

        Ok(keys)
    }

    fn mknod(
        &self,
        filename: &str,
        mode: InodeMode,
        dev_t: DeviceNumber,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let mut inode = self.0.lock();
        tmpfs_require_live_dir(&inode)?;

        let file_type = FileType::from(mode);
        if unlikely(file_type == FileType::File) {
            // Regular file creation must not recurse while holding the directory lock,
            // otherwise self.create() will try to lock the same Mutex and deadlock.
            drop(inode);
            return self.create(filename, FileType::File, mode);
        }

        let filename = DName::from(filename);

        // 确定文件类型
        let file_type = match file_type {
            FileType::Pipe => FileType::Pipe,
            FileType::CharDevice => FileType::CharDevice,
            FileType::BlockDevice => FileType::BlockDevice,
            FileType::Socket => FileType::Socket,
            _ => return Err(SystemError::EINVAL),
        };
        let init =
            crate::filesystem::vfs::permission::child_inode_init(&inode.metadata, file_type, mode);

        let now = PosixTimeSpec::now();
        let nod = Arc::new(LockedTmpfsInode::new(TmpfsInode {
            parent: inode.self_ref.clone(),
            self_ref: Weak::default(),
            children: BTreeMap::new(),
            page_cache: None,
            metadata: Metadata {
                dev_id: 0,
                inode_id: generate_inode_id(),
                size: 0,
                blk_size: 0,
                blocks: 0,
                atime: now,
                mtime: now,
                ctime: now,
                btime: now,
                file_type,
                mode: init.mode,
                nlinks: 1,
                uid: init.uid,
                gid: init.gid,
                raw_dev: dev_t,
                flags: InodeFlags::empty(),
            },
            fs: inode.fs.clone(),
            special_node: None,
            inline_symlink: None,
            name: filename.clone(),
        }));

        nod.0.lock().self_ref = Arc::downgrade(&nod);

        // 对于 FIFO，需要创建实际的 pipe inode
        if mode.contains(InodeMode::S_IFIFO) {
            let pipe_inode = LockedPipeInode::new();
            pipe_inode.set_fifo();
            nod.0.lock().special_node = Some(SpecialNodeData::Pipe(pipe_inode));
        }

        inode.children.insert(filename, nod.clone());
        tmpfs_touch_dir(&mut inode, now);
        Ok(nod)
    }

    fn special_node(&self) -> Option<super::vfs::SpecialNodeData> {
        self.0.lock().special_node.clone()
    }

    fn dname(&self) -> Result<DName, SystemError> {
        Ok(self.0.lock().name.clone())
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
