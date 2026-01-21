use core::any::Any;
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
    driver::base::device::device_number::DeviceNumber,
    filesystem::vfs::{vcore::generate_inode_id, FileType},
    ipc::pipe::LockedPipeInode,
    libs::casting::DowncastArc,
    libs::mutex::{Mutex, MutexGuard},
    mm::MemoryManagementArch,
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
    file::FilePrivateData, utils::DName, FileSystem, FsInfo, IndexNode, InodeFlags, InodeId,
    InodeMode, Metadata, SpecialNodeData,
};

use linkme::distributed_slice;

use super::vfs::{Magic, MountableFileSystem, SuperBlock};

const TMPFS_MAX_NAMELEN: usize = 255;
const TMPFS_BLOCK_SIZE: u64 = 4096;

const TMPFS_DEFAULT_MIN_SIZE_BYTES: usize = 16 * 1024 * 1024; // 16MiB
const TMPFS_DEFAULT_MAX_SIZE_BYTES: usize = 4 * 1024 * 1024 * 1024; // 4GiB

#[derive(Debug)]
struct TmpfsPageCacheBackend {
    inode: Weak<dyn IndexNode>,
}

impl TmpfsPageCacheBackend {
    fn new(inode: Weak<dyn IndexNode>) -> Self {
        Self { inode }
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
}

fn tmpfs_move_entry_between_dirs(
    src_dir: &mut TmpfsInode,
    dst_dir: &mut TmpfsInode,
    old_key: &DName,
    new_key: &DName,
    flags: RenameFlags,
) -> Result<(), SystemError> {
    if src_dir.metadata.file_type != FileType::Dir || dst_dir.metadata.file_type != FileType::Dir {
        return Err(SystemError::ENOTDIR);
    }

    let src_self = src_dir.self_ref.upgrade().ok_or(SystemError::EIO)?;
    let dst_self = dst_dir.self_ref.upgrade().ok_or(SystemError::EIO)?;

    let inode_to_move = src_dir
        .children
        .get(old_key)
        .cloned()
        .ok_or(SystemError::ENOENT)?;
    let old_type = inode_to_move.0.lock().metadata.file_type;

    if let Some(existing) = dst_dir.children.get(new_key) {
        if flags.contains(RenameFlags::NOREPLACE) {
            return Err(SystemError::EEXIST);
        }

        // Avoid self-deadlock: `existing` may be `src_dir`/`dst_dir` itself.
        if Arc::ptr_eq(existing, &src_self) {
            // Example: rename("dir/subdir", "dir") -> ENOTEMPTY (dir not empty).
            // Linux expects ENOTEMPTY for this case (TargetIsAncestorOfSource).
            return Err(SystemError::ENOTEMPTY);
        }
        if Arc::ptr_eq(existing, &dst_self) {
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
            return Ok(());
        }

        if old_type != existing_type {
            return Err(if old_type == FileType::Dir {
                SystemError::ENOTDIR
            } else {
                SystemError::EISDIR
            });
        }

        if old_type == FileType::Dir && existing_dir_nonempty {
            return Err(SystemError::ENOTEMPTY);
        }

        // Remove existing destination entry (replacement).
        dst_dir.children.remove(new_key);
        if old_type == FileType::Dir {
            dst_dir.metadata.nlinks = dst_dir.metadata.nlinks.saturating_sub(1);
        }
    }

    // Remove from source directory.
    src_dir.children.remove(old_key);
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

    Ok(())
}

#[derive(Debug)]
pub struct LockedTmpfsInode(pub Mutex<TmpfsInode>);

#[derive(Debug)]
pub struct Tmpfs {
    root_inode: Arc<LockedTmpfsInode>,
    super_block: RwSem<SuperBlock>,
    size_limit: Option<u64>,
    current_size: AtomicU64,
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
            name: Default::default(),
        }
    }
}

#[derive(Debug)]
pub struct TmpfsMountData {
    mode: InodeMode,
    size_bytes: Option<u64>,
}

impl TmpfsMountData {
    fn parse(raw: Option<&str>) -> Result<Self, SystemError> {
        let mut mode = InodeMode::S_IRWXUGO;
        let mut size_bytes = None;

        if let Some(raw) = raw {
            for opt in raw.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
                if let Some(v) = opt.strip_prefix("mode=").map(|s| s.trim()) {
                    // mode 参数按八进制解析（mount 的习惯用法，如 755 = rwxr-xr-x）
                    let parsed = u32::from_str_radix(v, 8).map_err(|_| SystemError::EINVAL)?;
                    mode = InodeMode::from_bits_truncate(parsed);
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
                    size_bytes = Some(base.saturating_mul(mul));
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
    unsafe fn fault(
        &self,
        pfm: &mut crate::mm::fault::PageFaultMessage,
    ) -> crate::mm::VmFaultReason {
        // tmpfs 是纯 page-cache 后端，不应走 pread/磁盘路径。
        PageFaultHandler::pagecache_fault_zero(pfm)
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

    fn super_block(&self) -> SuperBlock {
        self.super_block.read().clone()
    }

    fn support_readahead(&self) -> bool {
        // tmpfs 是内存文件系统，数据已经在 page_cache 中，不需要 readahead
        false
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

    fn update_superblock_free(&self, current_bytes: u64) {
        // 只在启用 size_limit 时输出可用容量；否则保持现有行为（0-sized）。
        let Some(limit) = self.size_limit else {
            return;
        };
        let total_blocks = limit / TMPFS_BLOCK_SIZE;
        let used_blocks = Self::bytes_to_blocks_ceil(current_bytes);
        let free_blocks = total_blocks.saturating_sub(used_blocks);
        let mut sb = self.super_block.write();
        sb.blocks = total_blocks;
        sb.bfree = free_blocks;
        sb.bavail = free_blocks;
        sb.frsize = TMPFS_BLOCK_SIZE;
    }

    pub fn new(mount_data: &TmpfsMountData) -> Arc<Self> {
        // 若未指定 size=，使用默认容量策略（通常为物理内存的一半）。
        // 这样 busybox df -h（默认过滤 f_blocks==0）就能显示 /tmp。
        let size_limit = mount_data
            .size_bytes
            .or_else(|| Some(Self::default_size_bytes() as u64));

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

        let root: Arc<LockedTmpfsInode> = Arc::new(LockedTmpfsInode(Mutex::new(TmpfsInode::new())));

        let result: Arc<Tmpfs> = Arc::new(Tmpfs {
            root_inode: root,
            super_block: RwSem::new(sb),
            size_limit,
            current_size: AtomicU64::new(0),
        });

        let mut root_guard: MutexGuard<TmpfsInode> = result.root_inode.0.lock();
        root_guard.parent = Arc::downgrade(&result.root_inode);
        root_guard.self_ref = Arc::downgrade(&result.root_inode);
        root_guard.fs = Arc::downgrade(&result);
        root_guard.metadata.mode = mount_data.mode;
        drop(root_guard);

        result
    }

    /// 原子地增加文件系统使用的大小
    /// 返回Ok(())如果更新成功，Err(SystemError::ENOSPC)如果超过限制
    /// 使用compare_exchange_weak循环确保并发安全
    fn increase_size(&self, size_diff: u64) -> Result<(), SystemError> {
        if let Some(limit) = self.size_limit {
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
                    Ok(_) => {
                        // 同步更新 superblock 的 free 统计，供 statfs/df 使用
                        self.update_superblock_free(new_total);
                        break;
                    } // 更新成功
                    Err(_) => continue, // 被其他线程修改，重试
                }
            }
        }
        Ok(())
    }

    /// 原子地减少文件系统当前使用的大小（用于文件删除或缩小）
    /// 使用fetch_sub确保并发安全
    fn decrease_size(&self, size: usize) {
        if self.size_limit.is_some() {
            let size_to_decrease = size as u64;
            // 使用fetch_sub原子地减少大小
            let prev = self
                .current_size
                .fetch_sub(size_to_decrease, Ordering::Release);
            let new = prev.saturating_sub(size_to_decrease);
            self.update_superblock_free(new);
        }
    }
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
            page: Arc<Page>,
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

            // tmpfs: 缺页即创建零页
            let page = page_cache.manager().commit_overwrite(page_index)?;

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

            let page_guard = it.page.read();
            unsafe {
                buf[dst_off..dst_off + it.sub_len].copy_from_slice(
                    &page_guard.as_slice()[it.page_offset..it.page_offset + it.sub_len],
                );
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
        let inode = self.0.lock();
        if inode.metadata.file_type == FileType::Dir {
            return Err(SystemError::EISDIR);
        }
        let page_cache = inode.page_cache.clone().ok_or(SystemError::EIO)?;
        let old_size = inode.metadata.size as usize;
        let new_size = (offset + len).max(old_size);
        let size_diff = new_size.saturating_sub(old_size) as u64;

        // 获取文件系统引用
        let fs = inode.fs.upgrade().ok_or(SystemError::EIO)?;
        let tmpfs = fs
            .as_any_ref()
            .downcast_ref::<Tmpfs>()
            .ok_or(SystemError::EIO)?;

        // 先预留空间，失败直接返回
        if size_diff > 0 {
            tmpfs.increase_size(size_diff)?;
        }

        drop(inode);

        let start_page_index = offset >> MMArch::PAGE_SHIFT;
        let end_page_index = (offset + len - 1) >> MMArch::PAGE_SHIFT;
        // 两阶段写入：同样避免在持有 page_cache 锁时触碰用户缓冲区（SelfRead）。
        struct WriteItem {
            page: Arc<Page>,
            page_index: usize,
            page_offset: usize,
            sub_len: usize,
        }

        let mut items: Vec<WriteItem> = Vec::new();
        for page_index in start_page_index..=end_page_index {
            let page_start = page_index * MMArch::PAGE_SIZE;
            let page_end = page_start + MMArch::PAGE_SIZE;

            let write_start = core::cmp::max(offset, page_start);
            let write_end = core::cmp::min(offset + len, page_end);
            let page_write_len = write_end.saturating_sub(write_start);
            if page_write_len == 0 {
                continue;
            }

            let page = page_cache.manager().commit_overwrite(page_index)?;

            items.push(WriteItem {
                page,
                page_index,
                page_offset: write_start - page_start,
                sub_len: page_write_len,
            });
        }

        let mut src_off = 0usize;
        for it in items {
            if it.sub_len == 0 {
                continue;
            }

            // prefault 用户缓冲区，避免后续在持页锁时缺页
            volatile_read!(buf[src_off]);
            volatile_read!(buf[src_off + it.sub_len - 1]);

            let mut page_guard = it.page.write();
            unsafe {
                page_guard.as_slice_mut()[it.page_offset..it.page_offset + it.sub_len]
                    .copy_from_slice(&buf[src_off..src_off + it.sub_len]);
            }
            page_guard.add_flags(crate::mm::page::PageFlags::PG_DIRTY);
            page_cache.manager().update_page(it.page_index)?;
            src_off += it.sub_len;
        }

        // 更新文件大小
        let mut inode = self.0.lock();
        if new_size > old_size {
            inode.metadata.size = new_size as i64;
        }
        Ok(len)
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

    fn resize(&self, len: usize) -> Result<(), SystemError> {
        let mut inode = self.0.lock();
        if inode.metadata.file_type == FileType::File {
            let old_size = inode.metadata.size as usize;
            let new_size = len;
            let size_diff = new_size.saturating_sub(old_size) as i64;

            // 获取文件系统引用
            let fs = inode.fs.upgrade().ok_or(SystemError::EIO)?;
            let tmpfs = fs
                .as_any_ref()
                .downcast_ref::<Tmpfs>()
                .ok_or(SystemError::EIO)?;

            // 如果扩大，原子地预留空间
            if size_diff > 0 {
                tmpfs.increase_size(size_diff as u64)?;
            }

            // 调整页缓存（会释放多余页，并截断最后一页）
            if let Some(pc) = inode.page_cache.clone() {
                pc.lock().resize(len)?;
            }

            // 如果缩小，减少current_size
            if size_diff < 0 {
                tmpfs.decrease_size((-size_diff) as usize);
            }

            inode.metadata.size = len as i64;

            Ok(())
        } else {
            Err(SystemError::EINVAL)
        }
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
        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }
        if inode.children.contains_key(&name) {
            return Err(SystemError::EEXIST);
        }

        let result: Arc<LockedTmpfsInode> = Arc::new(LockedTmpfsInode(Mutex::new(TmpfsInode {
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
                atime: PosixTimeSpec::default(),
                mtime: PosixTimeSpec::default(),
                ctime: PosixTimeSpec::default(),
                btime: PosixTimeSpec::default(),
                file_type,
                mode,
                flags: InodeFlags::empty(),
                nlinks: if file_type == FileType::Dir { 2 } else { 1 },
                uid: 0,
                gid: 0,
                raw_dev: DeviceNumber::from(data as u32),
            },
            fs: inode.fs.clone(),
            special_node: None,
            name: name.clone(),
        })));

        result.0.lock().self_ref = Arc::downgrade(&result);

        // tmpfs 中：普通文件和符号链接都需要可读写的数据存储。
        // 目前 VFS 使用 read_at/write_at 来读写 symlink 内容（readlink/symlink 语义），
        // 因此 symlink 也必须有 page_cache 后端，否则会在 write_at/read_at 返回 EIO。
        if file_type == FileType::File || file_type == FileType::SymLink {
            let backend = Arc::new(TmpfsPageCacheBackend::new(
                Arc::downgrade(&result) as Weak<dyn IndexNode>
            ));
            let pc = PageCache::new(
                Some(Arc::downgrade(&result) as Weak<dyn IndexNode>),
                Some(backend),
            );
            pc.set_unevictable(true);
            pc.set_shmem(true);
            result.0.lock().page_cache = Some(pc);
        }

        inode.children.insert(name, result.clone());
        if file_type == FileType::Dir {
            inode.metadata.nlinks += 1;
        }
        Ok(result)
    }

    fn link(&self, name: &str, other: &Arc<dyn IndexNode>) -> Result<(), SystemError> {
        let other: &LockedTmpfsInode = other
            .downcast_ref::<LockedTmpfsInode>()
            .ok_or(SystemError::EPERM)?;
        let name = DName::from(name);
        let mut inode: MutexGuard<TmpfsInode> = self.0.lock();
        let mut other_locked: MutexGuard<TmpfsInode> = other.0.lock();

        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }
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
        Ok(())
    }

    fn unlink(&self, name: &str) -> Result<(), SystemError> {
        let mut inode: MutexGuard<TmpfsInode> = self.0.lock();
        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }
        if name == "." || name == ".." {
            return Err(SystemError::ENOTEMPTY);
        }

        let name = DName::from(name);
        let to_delete = inode.children.get(&name).ok_or(SystemError::ENOENT)?;
        let deleted_inode = to_delete.0.lock();
        if deleted_inode.metadata.file_type == FileType::Dir {
            return Err(SystemError::EPERM);
        }

        // 获取文件大小，用于减少current_size
        let file_size = deleted_inode.metadata.size as usize;
        let fs = deleted_inode.fs.upgrade().ok_or(SystemError::EIO)?;
        let tmpfs = fs
            .as_any_ref()
            .downcast_ref::<Tmpfs>()
            .ok_or(SystemError::EIO)?;

        drop(deleted_inode);
        to_delete.0.lock().metadata.nlinks -= 1;
        inode.children.remove(&name);

        // 减少文件系统使用的大小
        tmpfs.decrease_size(file_size);

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
        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }
        let to_delete = inode.children.get(&name).ok_or(SystemError::ENOENT)?;
        let deleted_inode = to_delete.0.lock();
        if deleted_inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        // 检查目录是否为空（排除 "." 和 ".."）
        if !deleted_inode.children.is_empty() {
            return Err(SystemError::ENOTEMPTY);
        }

        // 目录的大小通常是0（不包含数据），但为了完整性，我们也处理
        let dir_size = deleted_inode.metadata.size as usize;
        let fs = deleted_inode.fs.upgrade().ok_or(SystemError::EIO)?;
        let tmpfs = fs
            .as_any_ref()
            .downcast_ref::<Tmpfs>()
            .ok_or(SystemError::EIO)?;

        drop(deleted_inode);
        to_delete.0.lock().metadata.nlinks -= 1;
        inode.children.remove(&name);
        inode.metadata.nlinks -= 1;

        // 减少文件系统使用的大小（目录通常大小为0）
        tmpfs.decrease_size(dir_size);

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

        // Fast path: renaming to itself.
        if Arc::ptr_eq(&(self.0.lock().self_ref.upgrade().unwrap()), &target_locked)
            && old_key == new_key
        {
            return Ok(());
        }

        // Lock ordering: lock by inode_id to avoid deadlocks.
        let self_id = self.0.lock().metadata.inode_id;
        let target_id = target_locked.0.lock().metadata.inode_id;

        if self_id == target_id {
            // Same directory rename.
            let mut dir = self.0.lock();
            let inode_to_move = dir
                .children
                .get(&old_key)
                .cloned()
                .ok_or(SystemError::ENOENT)?;
            let old_type = inode_to_move.0.lock().metadata.file_type;

            if let Some(existing) = dir.children.get(&new_key) {
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
                if old_type != existing_type {
                    return Err(if old_type == FileType::Dir {
                        SystemError::ENOTDIR
                    } else {
                        SystemError::EISDIR
                    });
                }

                if old_type == FileType::Dir && !existing.0.lock().children.is_empty() {
                    return Err(SystemError::ENOTEMPTY);
                }

                // Remove existing destination entry (replacement).
                dir.children.remove(&new_key);
            }

            // Move entry within the same directory.
            dir.children.remove(&old_key);
            dir.children.insert(new_key.clone(), inode_to_move.clone());
            inode_to_move.0.lock().name = new_key;
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
        _dev_t: DeviceNumber,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let mut inode = self.0.lock();
        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        if unlikely(mode.contains(InodeMode::S_IFREG)) {
            // Regular file creation must not recurse while holding the directory lock,
            // otherwise self.create() will try to lock the same Mutex and deadlock.
            drop(inode);
            return self.create(filename, FileType::File, mode);
        }

        let filename = DName::from(filename);

        let nod = Arc::new(LockedTmpfsInode(Mutex::new(TmpfsInode {
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
                atime: PosixTimeSpec::default(),
                mtime: PosixTimeSpec::default(),
                ctime: PosixTimeSpec::default(),
                btime: PosixTimeSpec::default(),
                file_type: FileType::Pipe,
                mode,
                nlinks: 1,
                uid: 0,
                gid: 0,
                raw_dev: DeviceNumber::default(),
                flags: InodeFlags::empty(),
            },
            fs: inode.fs.clone(),
            special_node: None,
            name: filename.clone(),
        })));

        nod.0.lock().self_ref = Arc::downgrade(&nod);

        if mode.contains(InodeMode::S_IFIFO) {
            nod.0.lock().metadata.file_type = FileType::Pipe;
            let pipe_inode = LockedPipeInode::new();
            pipe_inode.set_fifo();
            nod.0.lock().special_node = Some(SpecialNodeData::Pipe(pipe_inode));
        } else if mode.contains(InodeMode::S_IFBLK) || mode.contains(InodeMode::S_IFCHR) {
            return Err(SystemError::ENOSYS);
        }

        inode.children.insert(filename, nod.clone());
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
