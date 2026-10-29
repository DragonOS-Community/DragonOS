use core::any::Any;
use core::intrinsics::unlikely;

use crate::filesystem::page_cache::{PageCache, PageCacheBackend};
use crate::filesystem::vfs::syscall::RenameFlags;
use crate::filesystem::vfs::{FileSystemMakerData, FsCreationContext, FSMAKER};
use crate::libs::rwsem::RwSem;
use crate::mm::allocation::MemoryAllocationError;
use crate::mm::fault::{PageFaultHandler, PageFaultMessage};
use crate::mm::VmFaultReason;
use crate::process::namespace::user_namespace::{UserNamespace, INIT_USER_NAMESPACE};
use crate::register_mountable_fs;
use crate::{
    arch::MMArch,
    driver::base::device::device_number::{DeviceNumber, Major},
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
    file::FilePrivateData, utils::DName, FileSystem, FsInfo, FsReconfigureRequest, IndexNode,
    InodeFlags, InodeId, InodeMode, LinkMutationCoordinator, LinkRemovalOutcome, Metadata,
    MetadataUpdate, RenameOutcome, SetMetadataMask, SpecialNodeData,
};

use linkme::distributed_slice;

use super::vfs::permission::InodeOpContext;
use super::vfs::{Magic, MountableFileSystem, OpenTruncateContext, SuperBlock};

/// RamFS的inode名称的最大长度
const RAMFS_MAX_NAMELEN: usize = 64;
const RAMFS_BLOCK_SIZE: u64 = 512;
const WHITEOUT_DEV: DeviceNumber = DeviceNumber::new(Major::UNNAMED_MAJOR, 0);

fn ramfs_move_entry_between_dirs(
    src_dir: &mut RamFSInode,
    dst_dir: &mut RamFSInode,
    old_key: &DName,
    new_key: &DName,
    flags: RenameFlags,
) -> Result<RenameOutcome, SystemError> {
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

    if flags.contains(RenameFlags::EXCHANGE) {
        let existing = dst_dir
            .children
            .get(new_key)
            .cloned()
            .ok_or(SystemError::ENOENT)?;
        if Arc::ptr_eq(&inode_to_move, &existing) {
            return Ok(RenameOutcome::NoOp);
        }
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
        }
        {
            let mut replaced = existing.0.lock();
            replaced.parent = Arc::downgrade(&src_self);
            replaced.name = old_key.clone();
        }
        return Ok(RenameOutcome::Exchange);
    }

    let mut replaced = None;
    if let Some(existing) = dst_dir.children.get(new_key).cloned() {
        if flags.contains(RenameFlags::NOREPLACE) {
            return Err(SystemError::EEXIST);
        }

        let (existing_id, existing_type, existing_dir_nonempty) = {
            let guard = existing.0.lock();
            let t = guard.metadata.file_type;
            let nonempty = t == FileType::Dir && !guard.children.is_empty();
            (guard.metadata.inode_id, t, nonempty)
        };
        let to_move_id = inode_to_move.0.lock().metadata.inode_id;
        if existing_id == to_move_id {
            return Ok(RenameOutcome::NoOp);
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

        dst_dir.children.remove(new_key);
        let mut existing_guard = existing.0.lock();
        if existing_type == FileType::Dir {
            dst_dir.metadata.nlinks = dst_dir.metadata.nlinks.saturating_sub(1);
            existing_guard.metadata.nlinks = 0;
            replaced = Some(LinkRemovalOutcome::LastLink);
        } else {
            existing_guard.metadata.nlinks = existing_guard.metadata.nlinks.saturating_sub(1);
            replaced = Some(if existing_guard.metadata.nlinks == 0 {
                LinkRemovalOutcome::LastLink
            } else {
                LinkRemovalOutcome::StillLinked
            });
        }
    }

    src_dir.children.remove(old_key);
    if flags.contains(RenameFlags::WHITEOUT) {
        ramfs_insert_whiteout(src_dir, old_key)?;
    }
    if old_type == FileType::Dir {
        src_dir.metadata.nlinks = src_dir.metadata.nlinks.saturating_sub(1);
        dst_dir.metadata.nlinks = dst_dir.metadata.nlinks.saturating_add(1);
    }
    dst_dir
        .children
        .insert(new_key.clone(), inode_to_move.clone());

    let mut moved = inode_to_move.0.lock();
    moved.parent = Arc::downgrade(&dst_self);
    moved.name = new_key.clone();
    Ok(RenameOutcome::Moved { replaced })
}

fn ramfs_insert_whiteout(dir: &mut RamFSInode, name: &DName) -> Result<(), SystemError> {
    if dir.children.contains_key(name) {
        return Err(SystemError::EEXIST);
    }

    let whiteout = Arc::new(LockedRamFSInode(
        Mutex::new(RamFSInode {
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
                atime: PosixTimeSpec::default(),
                mtime: PosixTimeSpec::default(),
                ctime: PosixTimeSpec::default(),
                btime: PosixTimeSpec::default(),
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
            name: name.clone(),
        }),
        LinkMutationCoordinator::new(),
        RwSem::new(()),
    ));
    whiteout.0.lock().self_ref = Arc::downgrade(&whiteout);
    dir.children.insert(name.clone(), whiteout);
    Ok(())
}

/// @brief 内存文件系统的Inode结构体
#[derive(Debug)]
pub struct LockedRamFSInode(pub Mutex<RamFSInode>, LinkMutationCoordinator, RwSem<()>);

/// @brief 内存文件系统结构体
#[derive(Debug)]
pub struct RamFS {
    /// RamFS的root inode
    root_inode: Arc<LockedRamFSInode>,
    super_block: RwSem<SuperBlock>,
    owner_user_ns: Arc<UserNamespace>,
}

#[derive(Debug)]
struct RamFSMountData {
    mode: InodeMode,
}

impl FileSystemMakerData for RamFSMountData {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl RamFSMountData {
    fn parse(raw: Option<&str>) -> Result<Self, SystemError> {
        let mut mode = InodeMode::S_IRWXU
            | InodeMode::S_IRGRP
            | InodeMode::S_IXGRP
            | InodeMode::S_IROTH
            | InodeMode::S_IXOTH;
        for option in raw.unwrap_or("").split(',') {
            // Linux ramfs ignores unknown options; mode is the only parameter.
            if let Some(value) = option.strip_prefix("mode=") {
                let bits = u32::from_str_radix(value, 8).map_err(|_| SystemError::EINVAL)?;
                mode = InodeMode::from_bits_truncate(bits & 0o7777);
            }
        }
        Ok(Self { mode })
    }
}

/// @brief 内存文件系统的Inode结构体(不包含锁)
#[derive(Debug)]
pub struct RamFSInode {
    // parent变量目前只在find函数中使用到
    // 所以只有当inode是文件夹的时候，parent才会生效
    // 对于文件来说，parent就没什么作用了
    // 关于parent的说明: 目录不允许有硬链接
    /// 指向父Inode的弱引用
    parent: Weak<LockedRamFSInode>,
    /// 指向自身的弱引用
    self_ref: Weak<LockedRamFSInode>,
    /// 子Inode的B树
    children: BTreeMap<DName, Arc<LockedRamFSInode>>,
    /// 当前inode的数据部分
    page_cache: Option<Arc<PageCache>>,
    /// 当前inode的元数据
    metadata: Metadata,
    /// 指向inode所在的文件系统对象的指针
    fs: Weak<RamFS>,
    /// 指向特殊节点
    special_node: Option<SpecialNodeData>,

    name: DName,
}

impl RamFSInode {
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
                // 根目录的链接计数至少为2（. 和 从父挂载的引用）
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
// Ramfs has no backing store: the shmem mapping is the sole owner of bytes.
#[derive(Debug)]
struct RamFSPageCacheBackend(Weak<dyn IndexNode>);

impl PageCacheBackend for RamFSPageCacheBackend {
    fn supports_writeback(&self) -> bool {
        false
    }

    fn read_page(&self, _index: usize, _buf: &mut [u8]) -> Result<usize, SystemError> {
        Ok(0)
    }

    fn write_page(&self, _index: usize, buf: &[u8]) -> Result<usize, SystemError> {
        Ok(buf.len())
    }

    fn npages(&self) -> usize {
        self.0
            .upgrade()
            .and_then(|inode| inode.metadata().ok())
            .map(|metadata| (metadata.size.max(0) as usize).div_ceil(MMArch::PAGE_SIZE))
            .unwrap_or(0)
    }
}

impl FileSystem for RamFS {
    fn cached_find_in_view(
        &self,
        inode: &Arc<dyn IndexNode>,
        name: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        inode.cached_find(name)
    }

    unsafe fn fault(&self, pfm: &mut PageFaultMessage) -> VmFaultReason {
        PageFaultHandler::pagecache_fault_zero(pfm)
    }

    unsafe fn page_mkwrite(&self, pfm: &mut PageFaultMessage) -> VmFaultReason {
        PageFaultHandler::filemap_page_mkwrite(pfm)
    }

    unsafe fn map_pages(
        &self,
        pfm: &mut PageFaultMessage,
        start: usize,
        end: usize,
    ) -> VmFaultReason {
        PageFaultHandler::filemap_map_pages(pfm, start, end)
    }

    fn page_cache_writeback_domain(
        &self,
    ) -> Option<&Arc<crate::filesystem::page_cache::PageCacheWritebackDomain>> {
        None
    }

    fn root_inode(&self) -> Arc<dyn super::vfs::IndexNode> {
        return self.root_inode.clone();
    }

    fn info(&self) -> FsInfo {
        return FsInfo {
            blk_dev_id: 0,
            max_name_len: RAMFS_MAX_NAMELEN,
        };
    }

    /// @brief 本函数用于实现动态转换。
    /// 具体的文件系统在实现本函数时，最简单的方式就是：直接返回self
    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        "ramfs"
    }

    fn super_block(&self) -> SuperBlock {
        self.super_block.read().clone()
    }

    fn mount_owner_user_ns(&self) -> Option<Arc<UserNamespace>> {
        Some(self.owner_user_ns.clone())
    }

    fn reconfigure(
        &self,
        request: FsReconfigureRequest<'_>,
    ) -> Result<super::vfs::mount::MountFlags, SystemError> {
        if let Some(raw) = request.raw_data {
            for opt in raw.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
                if let Some(v) = opt.strip_prefix("mode=").map(|s| s.trim()) {
                    let _ = u32::from_str_radix(v, 8).map_err(|_| SystemError::EINVAL)?;
                }
            }
        }
        Ok(request.sb_flags & request.sb_flags_mask)
    }
}

impl RamFS {
    pub fn new() -> Arc<Self> {
        // Kernel users retain the historical root identity and permissions.
        Self::new_in_context(InodeMode::S_IRWXUGO, 0, 0, INIT_USER_NAMESPACE.clone())
    }

    fn new_in_context(
        mode: InodeMode,
        uid: usize,
        gid: usize,
        owner: Arc<UserNamespace>,
    ) -> Arc<Self> {
        let super_block = SuperBlock::new(
            Magic::RAMFS_MAGIC,
            RAMFS_BLOCK_SIZE,
            RAMFS_MAX_NAMELEN as u64,
        );
        // 初始化root inode
        let root: Arc<LockedRamFSInode> = Arc::new(LockedRamFSInode(
            Mutex::new(RamFSInode::new()),
            LinkMutationCoordinator::new(),
            RwSem::new(()),
        ));

        let result: Arc<RamFS> = Arc::new(RamFS {
            root_inode: root,
            super_block: RwSem::new(super_block),
            owner_user_ns: owner,
        });

        // 对root inode加锁，并继续完成初始化工作
        let mut root_guard: MutexGuard<RamFSInode> = result.root_inode.0.lock();
        root_guard.parent = Arc::downgrade(&result.root_inode);
        root_guard.self_ref = Arc::downgrade(&result.root_inode);
        root_guard.fs = Arc::downgrade(&result);
        root_guard.metadata.mode = mode;
        root_guard.metadata.uid = uid;
        root_guard.metadata.gid = gid;
        // 释放锁
        drop(root_guard);

        return result;
    }
}

impl MountableFileSystem for RamFS {
    const SUPPORTS_USERNS_MOUNT: bool = true;

    fn make_mount_data(
        raw_data: Option<&str>,
        _source: &str,
    ) -> Result<
        Option<Arc<dyn FileSystemMakerData + 'static>>,
        crate::mm::allocation::MemoryAllocationError,
    > {
        Ok(Some(Arc::new(RamFSMountData::parse(raw_data)?)))
    }
    fn make_fs(
        data: Option<&dyn FileSystemMakerData>,
    ) -> Result<Arc<dyn FileSystem + 'static>, SystemError> {
        let mode = data
            .and_then(|data| data.as_any().downcast_ref::<RamFSMountData>())
            .map(|data| data.mode)
            .unwrap_or(RamFSMountData::parse(None)?.mode);
        Ok(Self::new_in_context(
            mode,
            0,
            0,
            INIT_USER_NAMESPACE.clone(),
        ))
    }

    fn make_fs_in_context(
        data: Option<&dyn FileSystemMakerData>,
        _flags: super::vfs::mount::MountFlags,
        context: &FsCreationContext,
    ) -> Result<Arc<dyn FileSystem>, SystemError> {
        let data = data
            .and_then(|data| data.as_any().downcast_ref::<RamFSMountData>())
            .ok_or(SystemError::EINVAL)?;
        // Linux ramfs_get_inode uses inode_init_owner/current_fsuid at
        // get_tree, unlike tmpfs which explicitly uses fc->cred's saved IDs.
        let cred = crate::process::ProcessManager::current_pcb().cred();
        Ok(Self::new_in_context(
            data.mode,
            cred.fsuid.data(),
            cred.fsgid.data(),
            context.cred.user_ns.clone(),
        ))
    }
}

register_mountable_fs!(RamFS, RAMFSMAKER, "ramfs");

impl LockedRamFSInode {
    /// Size and write-side metadata share the existing content transaction.
    /// The inode mutex must not cross cache truncation, which takes MM locks.
    fn resize_with_operation(
        &self,
        len: usize,
        request: Option<(&Metadata, SetMetadataMask, &InodeOpContext)>,
    ) -> Result<SetMetadataMask, MemoryAllocationError> {
        if len > isize::MAX as usize {
            return Err(SystemError::EFBIG.into());
        }
        if request.is_some_and(|(_, _, context)| context.is_idmapped()) {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP.into());
        }
        let _content = self.2.write();
        let (cache, applied) = {
            let mut inode = self.0.lock();
            if inode.metadata.file_type != FileType::File {
                return Err(SystemError::EINVAL.into());
            }
            let cache = inode.page_cache.clone().ok_or(SystemError::EIO)?;
            let mut applied = SetMetadataMask::empty();
            if let Some((requested, mask, context)) = request {
                let (metadata, mask) = super::vfs::vcore::prepare_backing_resize_metadata(
                    context,
                    &inode.metadata,
                    requested,
                    mask,
                    len,
                )?;
                super::vfs::merge_metadata_masked(&mut inode.metadata, &metadata, mask);
                applied = mask;
            }
            // merge_metadata_masked deliberately does not apply size.
            inode.metadata.size = len as i64;
            (cache, applied)
        };
        // EOF precedes unmapping and tail clearing, including equal-size and
        // growing truncates. A later cache error retains that published EOF.
        cache.manager().resize(len)?;
        Ok(applied)
    }

    /// Prepare private contents before publishing a child in the namespace.
    /// Ordinary create has no initializer; a symlink must finish its page
    /// admission and target write before another lookup can observe it.
    fn create_child_with_init<E: From<SystemError>>(
        &self,
        name: &str,
        file_type: FileType,
        mode: InodeMode,
        data: usize,
        context: &super::vfs::permission::InodeOpContext,
        initialize: impl FnOnce(&Arc<LockedRamFSInode>) -> Result<(), E>,
    ) -> Result<Arc<dyn IndexNode>, E> {
        if context.is_idmapped() {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP.into());
        }
        let name = DName::from(name);
        let inode = self.0.lock();
        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR.into());
        }
        if inode.children.contains_key(&name) {
            return Err(SystemError::EEXIST.into());
        }
        super::vfs::permission::check_parent_create(&inode.metadata, context, false)?;
        let init = super::vfs::permission::child_inode_init_with_context(
            &inode.metadata,
            file_type,
            mode,
            context,
        )?;
        let parent = inode.self_ref.clone();
        let fs = inode.fs.clone();
        drop(inode);

        let result = Arc::try_new(LockedRamFSInode(
            Mutex::new(RamFSInode {
                parent,
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
                    mode: init.mode,
                    flags: InodeFlags::empty(),
                    nlinks: if file_type == FileType::Dir { 2 } else { 1 },
                    uid: init.uid,
                    gid: init.gid,
                    raw_dev: DeviceNumber::from(data as u32),
                },
                fs,
                special_node: None,
                name: name.clone(),
            }),
            LinkMutationCoordinator::new(),
            RwSem::new(()),
        ))
        .map_err(|_| SystemError::ENOMEM)?;
        result.0.lock().self_ref = Arc::downgrade(&result);
        if matches!(file_type, FileType::File | FileType::SymLink) {
            let inode_weak = Arc::downgrade(&result) as Weak<dyn IndexNode>;
            let backend = Arc::new(RamFSPageCacheBackend(inode_weak.clone()));
            let cache = PageCache::new_shmem(Some(inode_weak), Some(backend));
            cache.set_unevictable(true);
            result.0.lock().page_cache = Some(cache);
        }
        initialize(&result)?;

        // The initializer was lock-free with respect to the parent. Validate
        // the current namespace and inheritance state at the actual commit.
        let mut inode = self.0.lock();
        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR.into());
        }
        if inode.children.contains_key(&name) {
            return Err(SystemError::EEXIST.into());
        }
        super::vfs::permission::check_parent_create(&inode.metadata, context, false)?;
        let init = super::vfs::permission::child_inode_init_with_context(
            &inode.metadata,
            file_type,
            mode,
            context,
        )?;
        {
            let mut child = result.0.lock();
            child.metadata.mode = init.mode;
            child.metadata.uid = init.uid;
            child.metadata.gid = init.gid;
        }
        inode.children.insert(name, result.clone());
        if file_type == FileType::Dir {
            inode.metadata.nlinks += 1;
        }
        Ok(result)
    }
}

impl IndexNode for LockedRamFSInode {
    fn link_mutation_coordinator(&self) -> Option<&LinkMutationCoordinator> {
        Some(&self.1)
    }

    fn append_lock_fs(&self) -> Option<Arc<dyn FileSystem>> {
        Some(self.fs())
    }

    fn mmap(&self, _start: usize, _len: usize, _offset: usize) -> Result<(), SystemError> {
        Ok(())
    }

    fn truncate(&self, len: usize) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
        self.resize(len)
    }

    fn close(&self, _data: MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        return Ok(());
    }

    fn sync_file(
        &self,
        datasync: bool,
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
        match self.metadata()?.file_type {
            FileType::File | FileType::Dir => {
                if datasync {
                    self.datasync().map_err(Into::into)
                } else {
                    self.sync().map_err(Into::into)
                }
            }
            _ => Err(SystemError::EINVAL.into()),
        }
    }

    fn sync_file_range(
        &self,
        start: usize,
        end: usize,
        _datasync: bool,
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
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
            _ => Err(SystemError::EINVAL.into()),
        }
    }

    fn open(
        &self,
        _data: MutexGuard<FilePrivateData>,
        _mode: &super::vfs::file::FileFlags,
    ) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
        return Ok(());
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
        if buf.len() < len {
            return Err(SystemError::EINVAL.into());
        }
        offset.checked_add(len).ok_or(SystemError::EOVERFLOW)?;
        drop(_data);
        let cache = {
            let inode = self.0.lock();
            if inode.metadata.file_type == FileType::Dir {
                return Err(SystemError::EISDIR.into());
            }
            inode.page_cache.clone().ok_or(SystemError::EINVAL)?
        };
        let task = crate::process::ProcessManager::current_pcb();
        let owner = crate::cgroup::memory::account_for_task(&task);
        let creator = crate::cgroup::memory::object_for_task(&task);
        // The caller may still hold an append or stacked-filesystem guard.
        // Keep admission typed until that outer transaction has unwound.
        cache.try_read(offset, &mut buf[..len], &owner, &creator)
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
        let mut publish = || {};
        let mut stage = super::vfs::WritePrivilegeStage::new(&mut publish);
        self.write_at_with_privilege_stage(offset, len, buf, _data, &mut stage)
    }

    fn write_at_with_privilege_stage(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        data: MutexGuard<FilePrivateData>,
        stage: &mut super::vfs::WritePrivilegeStage<'_>,
    ) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
        self.write_at_with_inode_context(
            offset,
            len,
            buf,
            data,
            &super::vfs::permission::InodeOpContext::legacy(),
            stage,
        )
    }

    fn write_at_with_inode_context(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        data: MutexGuard<FilePrivateData>,
        context: &super::vfs::permission::InodeOpContext,
        stage: &mut super::vfs::WritePrivilegeStage<'_>,
    ) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
        self.write_primary_with_inode_context(
            offset,
            len,
            super::vfs::PrimaryWriteSource::Kernel(buf),
            crate::filesystem::vfs::WriteOperation::USER_BUFFERED,
            data,
            context,
            stage,
        )
        .map(|result| result.written_len)
    }

    fn write_primary_with_privilege_stage(
        &self,
        offset: usize,
        len: usize,
        source: super::vfs::PrimaryWriteSource<'_, '_>,
        operation: crate::filesystem::vfs::WriteOperation,
        data: MutexGuard<FilePrivateData>,
        stage: &mut super::vfs::WritePrivilegeStage<'_>,
    ) -> Result<super::vfs::PrimaryWriteResult, crate::mm::allocation::MemoryAllocationError> {
        self.write_primary_with_inode_context(
            offset,
            len,
            source,
            operation,
            data,
            &super::vfs::permission::InodeOpContext::legacy(),
            stage,
        )
    }

    fn write_primary_with_inode_context(
        &self,
        offset: usize,
        len: usize,
        source: super::vfs::PrimaryWriteSource<'_, '_>,
        _intent: crate::filesystem::vfs::WriteOperation,
        data: MutexGuard<FilePrivateData>,
        context: &super::vfs::permission::InodeOpContext,
        stage: &mut super::vfs::WritePrivilegeStage<'_>,
    ) -> Result<super::vfs::PrimaryWriteResult, crate::mm::allocation::MemoryAllocationError> {
        if context.is_idmapped() {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP.into());
        }
        if let super::vfs::PrimaryWriteSource::Kernel(buffer) = &source {
            if buffer.len() < len {
                return Err(SystemError::EINVAL.into());
            }
        }
        if len == 0 {
            return Ok(super::vfs::PrimaryWriteResult::new(0));
        }
        offset
            .checked_add(len)
            .filter(|end| *end <= isize::MAX as usize)
            .ok_or(SystemError::EFBIG)?;
        drop(data);
        // Metadata setters and truncate exclude the entire primary write.
        // Faults only read short metadata snapshots, so a same-file mapped
        // source can fault while this content guard is retained.
        let _content = self.2.read();
        let (cache, mode_changed) = {
            let mut inode = self.0.lock();
            if inode.metadata.file_type == FileType::Dir {
                return Err(SystemError::EISDIR.into());
            }
            let cache = inode.page_cache.clone().ok_or(SystemError::EINVAL)?;
            let mut mode_changed = false;
            if inode.metadata.file_type == FileType::File {
                if let Some(cred) = &context.cred {
                    let (requested, derived_mask) =
                        super::vfs::vcore::prepare_write_side_effect_metadata_with_cred(
                            context.view_metadata(&inode.metadata),
                            inode.metadata.size.max(0) as usize,
                            cred,
                        );
                    if derived_mask.contains(SetMetadataMask::MODE) {
                        let mask = SetMetadataMask::MODE
                            | SetMetadataMask::CTIME
                            | SetMetadataMask::WRITE_SIDE_EFFECT;
                        let backing =
                            context.backing_metadata_request(&inode.metadata, &requested, mask)?;
                        super::vfs::merge_metadata_masked(&mut inode.metadata, &backing, mask);
                        mode_changed = true;
                    }
                }
            }
            (cache, mode_changed)
        };
        // Publish committed permission changes even if the first user copy
        // faults. File completion must not repeat this stage after data.
        stage.complete();
        if mode_changed {
            stage.commit_attrib();
        }
        let task = crate::process::ProcessManager::current_pcb();
        let owner = crate::cgroup::memory::account_for_task(&task);
        let creator = crate::cgroup::memory::object_for_task(&task);
        source
            .write_chunks(len, |relative, bytes| {
                let position = offset.checked_add(relative).ok_or(SystemError::EFBIG)?;
                let end = position
                    .checked_add(bytes.len())
                    .ok_or(SystemError::EFBIG)?;
                cache.try_write_with_before_dirty(position, bytes, &owner, &creator, |_| {
                    let mut inode = self.0.lock();
                    inode.metadata.size = inode.metadata.size.max(end as i64);
                    Ok(())
                })
            })
            .map(super::vfs::PrimaryWriteResult::new)
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        return self.0.lock().fs.upgrade().unwrap();
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        let (mut metadata, cache) = {
            let inode = self.0.lock();
            (inode.metadata.clone(), inode.page_cache.clone())
        };
        if let Some(cache) = cache {
            metadata.blocks = cache
                .manager()
                .pages_count()?
                .checked_mul(MMArch::PAGE_SIZE / 512)
                .ok_or(SystemError::EOVERFLOW)?;
        }
        Ok(metadata)
    }

    fn cached_metadata(&self) -> Result<Metadata, SystemError> {
        let (mut metadata, cache) = {
            let inode = self
                .0
                .try_lock()
                .map_err(|_| SystemError::EAGAIN_OR_EWOULDBLOCK)?;
            (inode.metadata.clone(), inode.page_cache.clone())
        };
        if let Some(cache) = cache {
            metadata.blocks = cache
                .try_pages_count()?
                .checked_mul(MMArch::PAGE_SIZE / 512)
                .ok_or(SystemError::EOVERFLOW)?;
        }
        Ok(metadata)
    }

    fn page_cache(&self) -> Option<Arc<PageCache>> {
        self.0.lock().page_cache.clone()
    }

    fn set_metadata(
        &self,
        metadata: &Metadata,
    ) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
        let _content = self.2.write();
        let mut inode = self.0.lock();
        inode.metadata.atime = metadata.atime;
        inode.metadata.mtime = metadata.mtime;
        inode.metadata.ctime = metadata.ctime;
        inode.metadata.btime = metadata.btime;
        inode.metadata.mode = metadata.mode;
        inode.metadata.uid = metadata.uid;
        inode.metadata.gid = metadata.gid;

        return Ok(());
    }

    fn set_metadata_masked(
        &self,
        metadata: &Metadata,
        mask: SetMetadataMask,
    ) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
        let _content = self.2.write();
        let mut inode = self.0.lock();
        super::vfs::merge_metadata_masked(&mut inode.metadata, metadata, mask);
        Ok(())
    }

    fn update_metadata_masked(
        &self,
        update: &mut MetadataUpdate<'_>,
    ) -> Result<SetMetadataMask, crate::mm::allocation::MemoryAllocationError> {
        let _content = self.2.write();
        let current = self.0.lock().metadata.clone();
        let (requested, mask) = update(&current)?;
        let mut inode = self.0.lock();
        super::vfs::merge_metadata_masked(&mut inode.metadata, &requested, mask);
        Ok(mask)
    }

    fn update_atime(&self, now: PosixTimeSpec, relatime: bool) -> Result<(), SystemError> {
        let mut inode = self.0.lock();
        crate::filesystem::vfs::update_atime_locked(&mut inode.metadata, now, relatime);
        Ok(())
    }

    fn resize(&self, len: usize) -> Result<(), MemoryAllocationError> {
        self.resize_with_operation(len, None).map(|_| ())
    }

    fn resize_with_metadata(
        &self,
        len: usize,
        lock_owner: u64,
        metadata: &Metadata,
        mask: SetMetadataMask,
    ) -> Result<(), MemoryAllocationError> {
        self.resize_with_metadata_result(len, lock_owner, metadata, mask)
            .map(|_| ())
    }

    fn resize_with_metadata_result(
        &self,
        len: usize,
        lock_owner: u64,
        metadata: &Metadata,
        mask: SetMetadataMask,
    ) -> Result<SetMetadataMask, MemoryAllocationError> {
        self.resize_with_metadata_context(
            len,
            lock_owner,
            metadata,
            mask,
            &InodeOpContext::legacy(),
        )
    }

    fn resize_with_metadata_context(
        &self,
        len: usize,
        _lock_owner: u64,
        metadata: &Metadata,
        mask: SetMetadataMask,
        context: &InodeOpContext,
    ) -> Result<SetMetadataMask, MemoryAllocationError> {
        self.resize_with_operation(len, Some((metadata, mask, context)))
    }

    fn resize_file_with_metadata(
        &self,
        len: usize,
        lock_owner: u64,
        data: MutexGuard<FilePrivateData>,
        metadata: &Metadata,
        mask: SetMetadataMask,
    ) -> Result<(), MemoryAllocationError> {
        self.resize_file_with_metadata_result(len, lock_owner, data, metadata, mask)
            .map(|_| ())
    }

    fn resize_file_with_metadata_result(
        &self,
        len: usize,
        lock_owner: u64,
        data: MutexGuard<FilePrivateData>,
        metadata: &Metadata,
        mask: SetMetadataMask,
    ) -> Result<SetMetadataMask, MemoryAllocationError> {
        self.resize_file_with_metadata_context(
            len,
            lock_owner,
            data,
            metadata,
            mask,
            &InodeOpContext::legacy(),
        )
    }

    fn resize_file_with_metadata_context(
        &self,
        len: usize,
        lock_owner: u64,
        data: MutexGuard<FilePrivateData>,
        metadata: &Metadata,
        mask: SetMetadataMask,
        context: &InodeOpContext,
    ) -> Result<SetMetadataMask, MemoryAllocationError> {
        drop(data);
        self.resize_with_metadata_context(len, lock_owner, metadata, mask, context)
    }

    fn resize_open_truncate(
        &self,
        len: usize,
        lock_owner: u64,
        data: MutexGuard<FilePrivateData>,
        truncate: &OpenTruncateContext,
    ) -> Result<(), MemoryAllocationError> {
        self.resize_open_truncate_result(len, lock_owner, data, truncate)
            .map(|_| ())
    }

    fn resize_open_truncate_result(
        &self,
        len: usize,
        lock_owner: u64,
        data: MutexGuard<FilePrivateData>,
        truncate: &OpenTruncateContext,
    ) -> Result<SetMetadataMask, MemoryAllocationError> {
        self.resize_open_truncate_with_inode_context(
            len,
            lock_owner,
            data,
            truncate,
            &InodeOpContext::legacy(),
        )
    }

    fn resize_open_truncate_with_inode_context(
        &self,
        len: usize,
        lock_owner: u64,
        data: MutexGuard<FilePrivateData>,
        truncate: &OpenTruncateContext,
        context: &InodeOpContext,
    ) -> Result<SetMetadataMask, MemoryAllocationError> {
        self.resize_file_with_metadata_context(
            len,
            lock_owner,
            data,
            &truncate.requested,
            truncate.mask,
            context,
        )
    }

    fn fallocate_resize_atomic(
        &self,
        offset: usize,
        requested_end: usize,
        _lock_owner: u64,
    ) -> Result<SetMetadataMask, crate::mm::allocation::MemoryAllocationError> {
        if requested_end > isize::MAX as usize {
            return Err(SystemError::EFBIG.into());
        }
        let task = crate::process::ProcessManager::current_pcb();
        let owner = crate::cgroup::memory::account_for_task(&task);
        let creator = crate::cgroup::memory::object_for_task(&task);
        let result =
            (|| -> Result<SetMetadataMask, crate::mm::allocation::MemoryAllocationError> {
                let _size_guard = self.2.write();
                let cache = {
                    let inode = self.0.lock();
                    if inode.metadata.file_type != FileType::File {
                        return Err(SystemError::EINVAL.into());
                    }
                    if requested_end > inode.metadata.size as usize {
                        super::vfs::vcore::check_file_size_limit(requested_end)?;
                    }
                    inode.page_cache.clone().ok_or(SystemError::EIO)?
                };
                // Allocate only pages intersecting the request. Sparse holes before
                // offset must remain holes even when EOF already exceeds requested_end.
                cache.manager().try_preallocate_range(
                    offset >> MMArch::PAGE_SHIFT,
                    (requested_end - 1) >> MMArch::PAGE_SHIFT,
                    &owner,
                    &creator,
                )?;
                let mut inode = self.0.lock();
                let effective_size = (inode.metadata.size as usize).max(requested_end);
                let (metadata, mask) = super::vfs::vcore::prepare_write_side_effect_metadata(
                    inode.metadata.clone(),
                    effective_size,
                );
                crate::filesystem::vfs::merge_metadata_masked(&mut inode.metadata, &metadata, mask);
                inode.metadata.size = effective_size as i64;
                Ok(mask)
            })();
        result
    }

    fn fallocate_file(
        &self,
        mode: i32,
        offset: usize,
        len: usize,
        lock_owner: u64,
        attrib: &mut super::vfs::AttribStageObserver<'_>,
        data: MutexGuard<FilePrivateData>,
    ) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
        drop(data);
        super::vfs::vcore::resize_based_fallocate(self, mode, offset, len, lock_owner, attrib)
    }

    fn create_with_data(
        &self,
        name: &str,
        file_type: FileType,
        mode: InodeMode,
        data: usize,
    ) -> Result<Arc<dyn IndexNode>, crate::mm::allocation::MemoryAllocationError> {
        let mut context = super::vfs::permission::InodeOpContext::legacy();
        context.fs_userns = self.fs().mount_owner_user_ns().ok_or(SystemError::EIO)?;
        self.create_with_data_context(name, file_type, mode, data, &context)
    }

    fn create_with_context(
        &self,
        name: &str,
        file_type: FileType,
        mode: InodeMode,
        context: &super::vfs::permission::InodeOpContext,
    ) -> Result<Arc<dyn IndexNode>, crate::mm::allocation::MemoryAllocationError> {
        self.create_with_data_context(name, file_type, mode, 0, context)
    }

    fn mkdir_with_context(
        &self,
        name: &str,
        mode: InodeMode,
        context: &super::vfs::permission::InodeOpContext,
    ) -> Result<Arc<dyn IndexNode>, crate::mm::allocation::MemoryAllocationError> {
        self.create_with_context(name, FileType::Dir, mode, context)
    }

    fn symlink_with_context(
        &self,
        name: &str,
        target: &str,
        context: &super::vfs::permission::InodeOpContext,
    ) -> Result<Arc<dyn IndexNode>, crate::mm::allocation::MemoryAllocationError> {
        self.create_child_with_init(
            name,
            FileType::SymLink,
            InodeMode::S_IRWXUGO,
            0,
            context,
            |inode| {
                let written = inode.write_at(
                    0,
                    target.len(),
                    target.as_bytes(),
                    Mutex::new(FilePrivateData::Unused).lock(),
                )?;
                if written != target.len() {
                    return Err(SystemError::EIO.into());
                }
                Ok(())
            },
        )
    }

    fn create_with_data_context(
        &self,
        name: &str,
        file_type: FileType,
        mode: InodeMode,
        data: usize,
        context: &super::vfs::permission::InodeOpContext,
    ) -> Result<Arc<dyn IndexNode>, crate::mm::allocation::MemoryAllocationError> {
        self.create_child_with_init(name, file_type, mode, data, context, |_| Ok(()))
    }

    fn link(
        &self,
        name: &str,
        other: &Arc<dyn IndexNode>,
    ) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
        let other: &LockedRamFSInode = other
            .downcast_ref::<LockedRamFSInode>()
            .ok_or(SystemError::EINVAL)?;
        let name = DName::from(name);
        let mut inode: MutexGuard<RamFSInode> = self.0.lock();
        let mut other_locked: MutexGuard<RamFSInode> = other.0.lock();

        // 如果当前inode不是文件夹，那么报错
        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR.into());
        }

        // 如果另一个inode是文件夹，那么也报错
        if other_locked.metadata.file_type == FileType::Dir {
            return Err(SystemError::EISDIR.into());
        }

        // 如果当前文件夹下已经有同名文件，也报错。
        if inode.children.contains_key(&name) {
            return Err(SystemError::EEXIST.into());
        }

        inode
            .children
            .insert(name, other_locked.self_ref.upgrade().unwrap());

        // 增加硬链接计数
        other_locked.metadata.nlinks += 1;
        return Ok(());
    }

    fn unlink(
        &self,
        name: &str,
    ) -> Result<LinkRemovalOutcome, crate::mm::allocation::MemoryAllocationError> {
        let mut inode: MutexGuard<RamFSInode> = self.0.lock();
        // 如果当前inode不是目录，那么也没有子目录/文件的概念了，因此要求当前inode的类型是目录
        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR.into());
        }
        // 不允许删除当前文件夹，也不允许删除上一个目录
        if name == "." || name == ".." {
            return Err(SystemError::ENOTEMPTY.into());
        }

        let name = DName::from(name);
        // 获得要删除的文件的inode
        let to_delete = inode.children.get(&name).ok_or(SystemError::ENOENT)?;
        if to_delete.0.lock().metadata.file_type == FileType::Dir {
            return Err(SystemError::EPERM.into());
        }
        // 减少硬链接计数
        let mut deleted = to_delete.0.lock();
        deleted.metadata.nlinks = deleted
            .metadata
            .nlinks
            .checked_sub(1)
            .expect("ramfs nlinks underflow: filesystem corruption detected");
        let outcome = if deleted.metadata.nlinks == 0 {
            LinkRemovalOutcome::LastLink
        } else {
            LinkRemovalOutcome::StillLinked
        };
        drop(deleted);
        // 在当前目录中删除这个子目录项
        inode.children.remove(&name);
        return Ok(outcome);
    }

    fn rmdir(&self, name: &str) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
        let name = DName::from(name);
        let mut inode: MutexGuard<RamFSInode> = self.0.lock();
        // 如果当前inode不是目录，那么也没有子目录/文件的概念了，因此要求当前inode的类型是目录
        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR.into());
        }
        // 获得要删除的文件夹的inode
        let to_delete = inode.children.get(&name).ok_or(SystemError::ENOENT)?;
        if to_delete.0.lock().metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR.into());
        }

        to_delete.0.lock().metadata.nlinks -= 1;
        // 在当前目录中删除这个子目录项
        inode.children.remove(&name);
        // 父目录链接计数相应减少
        inode.metadata.nlinks -= 1;
        return Ok(());
    }

    fn move_to(
        &self,
        old_name: &str,
        target: &Arc<dyn IndexNode>,
        new_name: &str,
        flags: RenameFlags,
    ) -> Result<RenameOutcome, crate::mm::allocation::MemoryAllocationError> {
        let old_key = DName::from(old_name);
        let new_name = DName::from(new_name);
        let target_locked = target
            .clone()
            .downcast_arc::<LockedRamFSInode>()
            .ok_or(SystemError::EINVAL)?;

        let self_id = self.0.lock().metadata.inode_id;
        let target_id = target_locked.0.lock().metadata.inode_id;

        if self_id == target_id {
            let mut dir = self.0.lock();
            let inode_to_move = dir
                .children
                .get(&old_key)
                .cloned()
                .ok_or(SystemError::ENOENT)?;
            let old_type = inode_to_move.0.lock().metadata.file_type;

            if flags.contains(RenameFlags::EXCHANGE) {
                let existing = dir
                    .children
                    .get(&new_name)
                    .cloned()
                    .ok_or(SystemError::ENOENT)?;
                let to_move_id = inode_to_move.0.lock().metadata.inode_id;
                let existing_id = existing.0.lock().metadata.inode_id;
                if existing_id == to_move_id {
                    return Ok(RenameOutcome::NoOp);
                }

                dir.children.insert(old_key.clone(), existing.clone());
                dir.children.insert(new_name.clone(), inode_to_move.clone());
                existing.0.lock().name = old_key;
                inode_to_move.0.lock().name = new_name;
                return Ok(RenameOutcome::Exchange);
            }

            let mut replaced = None;
            if let Some(existing) = dir.children.get(&new_name).cloned() {
                if flags.contains(RenameFlags::NOREPLACE) {
                    return Err(SystemError::EEXIST.into());
                }

                let existing_id = existing.0.lock().metadata.inode_id;
                let to_move_id = inode_to_move.0.lock().metadata.inode_id;
                if existing_id == to_move_id {
                    return Ok(RenameOutcome::NoOp);
                }

                let existing_type = existing.0.lock().metadata.file_type;
                if old_type == FileType::Dir && existing_type != FileType::Dir {
                    return Err(SystemError::ENOTDIR.into());
                }
                if old_type != FileType::Dir && existing_type == FileType::Dir {
                    return Err(SystemError::EISDIR.into());
                }
                if old_type == FileType::Dir && !existing.0.lock().children.is_empty() {
                    return Err(SystemError::ENOTEMPTY.into());
                }

                dir.children.remove(&new_name);
                let mut existing_guard = existing.0.lock();
                if existing_type == FileType::Dir {
                    dir.metadata.nlinks = dir.metadata.nlinks.saturating_sub(1);
                    existing_guard.metadata.nlinks = 0;
                    replaced = Some(LinkRemovalOutcome::LastLink);
                } else {
                    existing_guard.metadata.nlinks =
                        existing_guard.metadata.nlinks.saturating_sub(1);
                    replaced = Some(if existing_guard.metadata.nlinks == 0 {
                        LinkRemovalOutcome::LastLink
                    } else {
                        LinkRemovalOutcome::StillLinked
                    });
                }
            }

            dir.children.remove(&old_key);
            if flags.contains(RenameFlags::WHITEOUT) {
                ramfs_insert_whiteout(&mut dir, &old_key)?;
            }
            dir.children.insert(new_name.clone(), inode_to_move.clone());
            inode_to_move.0.lock().name = new_name;
            return Ok(RenameOutcome::Moved { replaced });
        }

        if self_id < target_id {
            let mut src_dir = self.0.lock();
            let mut dst_dir = target_locked.0.lock();
            ramfs_move_entry_between_dirs(&mut src_dir, &mut dst_dir, &old_key, &new_name, flags)
                .map_err(Into::into)
        } else {
            let mut dst_dir = target_locked.0.lock();
            let mut src_dir = self.0.lock();
            ramfs_move_entry_between_dirs(&mut src_dir, &mut dst_dir, &old_key, &new_name, flags)
                .map_err(Into::into)
        }
    }

    fn find(&self, name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        let inode = self.0.lock();

        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        match name {
            "" | "." => {
                return Ok(inode.self_ref.upgrade().ok_or(SystemError::ENOENT)?);
            }

            ".." => {
                return Ok(inode.parent.upgrade().ok_or(SystemError::ENOENT)?);
            }
            name => {
                // 在子目录项中查找
                let name = DName::from(name);
                return Ok(inode
                    .children
                    .get(&name)
                    .ok_or(SystemError::ENOENT)?
                    .clone());
            }
        }
    }

    fn cached_find(&self, name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        let inode = self
            .0
            .try_lock()
            .map_err(|_| SystemError::EAGAIN_OR_EWOULDBLOCK)?;
        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }
        match name {
            "" | "." => inode
                .self_ref
                .upgrade()
                .map(|inode| inode as Arc<dyn IndexNode>)
                .ok_or(SystemError::ENOENT),
            ".." => inode
                .parent
                .upgrade()
                .map(|inode| inode as Arc<dyn IndexNode>)
                .ok_or(SystemError::ENOENT),
            name => inode
                .children
                .get(&DName::from(name))
                .cloned()
                .map(|child| child as Arc<dyn IndexNode>)
                .ok_or(SystemError::EAGAIN_OR_EWOULDBLOCK),
        }
    }

    fn get_entry_name(&self, ino: InodeId) -> Result<String, SystemError> {
        let inode: MutexGuard<RamFSInode> = self.0.lock();
        if inode.metadata.file_type != FileType::Dir {
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
                    0 => {
                        return Err(SystemError::ENOENT);
                    }
                    1 => {
                        return Ok(key.remove(0));
                    }
                    _ => panic!(
                        "Ramfs get_entry_name: key.len()={key_len}>1, current inode_id={inode_id:?}, to find={to_find:?}",
                        key_len = key.len(),
                        inode_id = inode.metadata.inode_id,
                        to_find = ino
                    ),
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

        return Ok(keys);
    }

    fn mknod(
        &self,
        filename: &str,
        mode: InodeMode,
        dev_t: DeviceNumber,
    ) -> Result<Arc<dyn IndexNode>, crate::mm::allocation::MemoryAllocationError> {
        let mut context = super::vfs::permission::InodeOpContext::legacy();
        context.fs_userns = self.fs().mount_owner_user_ns().ok_or(SystemError::EIO)?;
        self.mknod_with_context(filename, mode, dev_t, &context)
    }

    fn mknod_with_context(
        &self,
        filename: &str,
        mode: InodeMode,
        dev_t: DeviceNumber,
        context: &super::vfs::permission::InodeOpContext,
    ) -> Result<Arc<dyn IndexNode>, crate::mm::allocation::MemoryAllocationError> {
        if context.is_idmapped() {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP.into());
        }
        let mut inode = self.0.lock();
        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR.into());
        }

        // Regular file: delegate to create(), must drop lock first to avoid deadlock
        let file_type = FileType::from(mode);
        if unlikely(file_type == FileType::File) {
            drop(inode);
            return self.create_with_context(filename, FileType::File, mode, context);
        }

        let filename = DName::from(filename);
        if inode.children.contains_key(&filename) {
            return Err(SystemError::EEXIST.into());
        }

        // Determine file type from mode
        let file_type = match file_type {
            FileType::Pipe => FileType::Pipe,
            FileType::CharDevice => FileType::CharDevice,
            FileType::BlockDevice => FileType::BlockDevice,
            FileType::Socket => FileType::Socket,
            _ => return Err(SystemError::EINVAL.into()),
        };
        super::vfs::permission::check_parent_create(&inode.metadata, context, false)?;
        let init = super::vfs::permission::child_inode_init_with_context(
            &inode.metadata,
            file_type,
            mode,
            context,
        )?;

        let nod = Arc::new(LockedRamFSInode(
            Mutex::new(RamFSInode {
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
                    mode: init.mode,
                    nlinks: 1,
                    uid: init.uid,
                    gid: init.gid,
                    raw_dev: dev_t,
                    flags: InodeFlags::empty(),
                },
                fs: inode.fs.clone(),
                special_node: None,
                name: filename.clone(),
            }),
            LinkMutationCoordinator::new(),
            RwSem::new(()),
        ));

        nod.0.lock().self_ref = Arc::downgrade(&nod);

        // FIFO requires creating an actual pipe inode
        if mode.contains(InodeMode::S_IFIFO) {
            let pipe_inode = LockedPipeInode::new_fifo_shell();
            // 标记为命名管道（FIFO），这样 open 时才会应用 FIFO 阻塞语义
            pipe_inode.set_fifo();
            // 设置special_node
            nod.0.lock().special_node = Some(SpecialNodeData::Pipe(pipe_inode));
        }

        inode.children.insert(filename, nod.clone());
        Ok(nod)
    }

    fn special_node(&self) -> Option<super::vfs::SpecialNodeData> {
        return self.0.lock().special_node.clone();
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
}
