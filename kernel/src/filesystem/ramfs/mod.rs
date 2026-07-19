use core::any::Any;
use core::intrinsics::unlikely;

use crate::filesystem::vfs::syscall::RenameFlags;
use crate::filesystem::vfs::{FileSystemMakerData, FSMAKER};
use crate::libs::rwsem::RwSem;
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
    InodeFlags, InodeId, InodeMode, Metadata, SpecialNodeData,
};

use linkme::distributed_slice;

use super::vfs::{Magic, MountableFileSystem, SuperBlock};

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

    if flags.contains(RenameFlags::EXCHANGE) {
        let existing = dst_dir
            .children
            .get(new_key)
            .cloned()
            .ok_or(SystemError::ENOENT)?;
        if Arc::ptr_eq(&inode_to_move, &existing) {
            return Ok(());
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
        return Ok(());
    }

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
            src_dir.children.remove(old_key);
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

        dst_dir.children.remove(new_key);
        let mut existing_guard = existing.0.lock();
        if existing_type == FileType::Dir {
            dst_dir.metadata.nlinks = dst_dir.metadata.nlinks.saturating_sub(1);
            existing_guard.metadata.nlinks = 0;
        } else {
            existing_guard.metadata.nlinks = existing_guard.metadata.nlinks.saturating_sub(1);
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
    Ok(())
}

fn ramfs_insert_whiteout(dir: &mut RamFSInode, name: &DName) -> Result<(), SystemError> {
    if dir.children.contains_key(name) {
        return Err(SystemError::EEXIST);
    }

    let whiteout = Arc::new(LockedRamFSInode(Mutex::new(RamFSInode {
        parent: dir.self_ref.clone(),
        self_ref: Weak::default(),
        children: BTreeMap::new(),
        data: Vec::new(),
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
    })));
    whiteout.0.lock().self_ref = Arc::downgrade(&whiteout);
    dir.children.insert(name.clone(), whiteout);
    Ok(())
}

/// @brief 内存文件系统的Inode结构体
#[derive(Debug)]
pub struct LockedRamFSInode(pub Mutex<RamFSInode>);

/// @brief 内存文件系统结构体
#[derive(Debug)]
pub struct RamFS {
    /// RamFS的root inode
    root_inode: Arc<LockedRamFSInode>,
    super_block: RwSem<SuperBlock>,
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
    data: Vec<u8>,
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
            data: Vec::new(),
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
impl FileSystem for RamFS {
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
        let super_block = SuperBlock::new(
            Magic::RAMFS_MAGIC,
            RAMFS_BLOCK_SIZE,
            RAMFS_MAX_NAMELEN as u64,
        );
        // 初始化root inode
        let root: Arc<LockedRamFSInode> = Arc::new(LockedRamFSInode(Mutex::new(RamFSInode::new())));

        let result: Arc<RamFS> = Arc::new(RamFS {
            root_inode: root,
            super_block: RwSem::new(super_block),
        });

        // 对root inode加锁，并继续完成初始化工作
        let mut root_guard: MutexGuard<RamFSInode> = result.root_inode.0.lock();
        root_guard.parent = Arc::downgrade(&result.root_inode);
        root_guard.self_ref = Arc::downgrade(&result.root_inode);
        root_guard.fs = Arc::downgrade(&result);
        // 释放锁
        drop(root_guard);

        return result;
    }
}

impl MountableFileSystem for RamFS {
    fn make_mount_data(
        _raw_data: Option<&str>,
        _source: &str,
    ) -> Result<Option<Arc<dyn FileSystemMakerData + 'static>>, SystemError> {
        // 目前ramfs不需要任何额外的mount数据
        Ok(None)
    }
    fn make_fs(
        _data: Option<&dyn FileSystemMakerData>,
    ) -> Result<Arc<dyn FileSystem + 'static>, SystemError> {
        let fs = RamFS::new();
        return Ok(fs);
    }
}

register_mountable_fs!(RamFS, RAMFSMAKER, "ramfs");

impl IndexNode for LockedRamFSInode {
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
        let mut inode = self.0.lock();

        //如果是文件夹，则报错
        if inode.metadata.file_type == FileType::Dir {
            return Err(SystemError::EINVAL);
        }

        //当前文件长度大于_len才进行截断，否则不操作
        if inode.data.len() > len {
            inode.data.resize(len, 0);
        }
        return Ok(());
    }

    fn close(&self, _data: MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        return Ok(());
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
        return Ok(());
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
        // 加锁
        let inode: MutexGuard<RamFSInode> = self.0.lock();

        // 检查当前inode是否为一个文件夹，如果是的话，就返回错误
        if inode.metadata.file_type == FileType::Dir {
            return Err(SystemError::EISDIR);
        }

        let start = inode.data.len().min(offset);
        let end = inode.data.len().min(offset + len);

        // buffer空间不足
        if buf.len() < (end - start) {
            return Err(SystemError::ENOBUFS);
        }

        // 拷贝数据
        let src = &inode.data[start..end];
        buf[0..src.len()].copy_from_slice(src);
        return Ok(src.len());
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

        // 加锁
        let mut inode: MutexGuard<RamFSInode> = self.0.lock();

        // 检查当前inode是否为一个文件夹，如果是的话，就返回错误
        if inode.metadata.file_type == FileType::Dir {
            return Err(SystemError::EISDIR);
        }

        let data: &mut Vec<u8> = &mut inode.data;

        // 如果文件大小比原来的大，那就resize这个数组
        if offset + len > data.len() {
            data.resize(offset + len, 0);
        }

        let target = &mut data[offset..offset + len];
        target.copy_from_slice(&buf[0..len]);
        return Ok(len);
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        return self.0.lock().fs.upgrade().unwrap();
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        let inode = self.0.lock();
        let mut metadata = inode.metadata.clone();
        metadata.size = inode.data.len() as i64;

        return Ok(metadata);
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

        return Ok(());
    }

    fn update_atime(&self, now: PosixTimeSpec, relatime: bool) -> Result<(), SystemError> {
        let mut inode = self.0.lock();
        crate::filesystem::vfs::update_atime_locked(&mut inode.metadata, now, relatime);
        Ok(())
    }

    fn resize(&self, len: usize) -> Result<(), SystemError> {
        let mut inode = self.0.lock();
        if inode.metadata.file_type == FileType::File {
            inode.data.resize(len, 0);
            return Ok(());
        } else {
            return Err(SystemError::EINVAL);
        }
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
        super::vfs::vcore::resize_based_fallocate(self, mode, offset, len, lock_owner)
    }

    fn create_with_data(
        &self,
        name: &str,
        file_type: FileType,
        mode: InodeMode,
        data: usize,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let name = DName::from(name);
        // 获取当前inode
        let mut inode = self.0.lock();
        // 如果当前inode不是文件夹，则返回
        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }
        // 如果有重名的，则返回
        if inode.children.contains_key(&name) {
            return Err(SystemError::EEXIST);
        }
        let init =
            crate::filesystem::vfs::permission::child_inode_init(&inode.metadata, file_type, mode);

        // 创建inode
        let result: Arc<LockedRamFSInode> = Arc::new(LockedRamFSInode(Mutex::new(RamFSInode {
            parent: inode.self_ref.clone(),
            self_ref: Weak::default(),
            children: BTreeMap::new(),
            data: Vec::new(),
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
                // 目录需要包含 "." 自引用，因此初始为2
                nlinks: if file_type == FileType::Dir { 2 } else { 1 },
                uid: init.uid,
                gid: init.gid,
                raw_dev: DeviceNumber::from(data as u32),
            },
            fs: inode.fs.clone(),
            special_node: None,
            name: name.clone(),
        })));

        // 初始化inode的自引用的weak指针
        result.0.lock().self_ref = Arc::downgrade(&result);

        // 将子inode插入父inode的B树中
        inode.children.insert(name, result.clone());
        // 如果新建的是目录，父目录的 nlink 需要增加
        if file_type == FileType::Dir {
            inode.metadata.nlinks += 1;
        }

        return Ok(result);
    }

    fn link(&self, name: &str, other: &Arc<dyn IndexNode>) -> Result<(), SystemError> {
        let other: &LockedRamFSInode = other
            .downcast_ref::<LockedRamFSInode>()
            .ok_or(SystemError::EINVAL)?;
        let name = DName::from(name);
        let mut inode: MutexGuard<RamFSInode> = self.0.lock();
        let mut other_locked: MutexGuard<RamFSInode> = other.0.lock();

        // 如果当前inode不是文件夹，那么报错
        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        // 如果另一个inode是文件夹，那么也报错
        if other_locked.metadata.file_type == FileType::Dir {
            return Err(SystemError::EISDIR);
        }

        // 如果当前文件夹下已经有同名文件，也报错。
        if inode.children.contains_key(&name) {
            return Err(SystemError::EEXIST);
        }

        inode
            .children
            .insert(name, other_locked.self_ref.upgrade().unwrap());

        // 增加硬链接计数
        other_locked.metadata.nlinks += 1;
        return Ok(());
    }

    fn unlink(&self, name: &str) -> Result<(), SystemError> {
        let mut inode: MutexGuard<RamFSInode> = self.0.lock();
        // 如果当前inode不是目录，那么也没有子目录/文件的概念了，因此要求当前inode的类型是目录
        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }
        // 不允许删除当前文件夹，也不允许删除上一个目录
        if name == "." || name == ".." {
            return Err(SystemError::ENOTEMPTY);
        }

        let name = DName::from(name);
        // 获得要删除的文件的inode
        let to_delete = inode.children.get(&name).ok_or(SystemError::ENOENT)?;
        if to_delete.0.lock().metadata.file_type == FileType::Dir {
            return Err(SystemError::EPERM);
        }
        // 减少硬链接计数
        to_delete.0.lock().metadata.nlinks -= 1;
        // 在当前目录中删除这个子目录项
        inode.children.remove(&name);
        return Ok(());
    }

    fn rmdir(&self, name: &str) -> Result<(), SystemError> {
        let name = DName::from(name);
        let mut inode: MutexGuard<RamFSInode> = self.0.lock();
        // 如果当前inode不是目录，那么也没有子目录/文件的概念了，因此要求当前inode的类型是目录
        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }
        // 获得要删除的文件夹的inode
        let to_delete = inode.children.get(&name).ok_or(SystemError::ENOENT)?;
        if to_delete.0.lock().metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
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
    ) -> Result<(), SystemError> {
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
                    return Ok(());
                }

                dir.children.insert(old_key.clone(), existing.clone());
                dir.children.insert(new_name.clone(), inode_to_move.clone());
                existing.0.lock().name = old_key;
                inode_to_move.0.lock().name = new_name;
                return Ok(());
            }

            if let Some(existing) = dir.children.get(&new_name).cloned() {
                if flags.contains(RenameFlags::NOREPLACE) {
                    return Err(SystemError::EEXIST);
                }

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

                dir.children.remove(&new_name);
                let mut existing_guard = existing.0.lock();
                if existing_type == FileType::Dir {
                    dir.metadata.nlinks = dir.metadata.nlinks.saturating_sub(1);
                    existing_guard.metadata.nlinks = 0;
                } else {
                    existing_guard.metadata.nlinks =
                        existing_guard.metadata.nlinks.saturating_sub(1);
                }
            }

            dir.children.remove(&old_key);
            if flags.contains(RenameFlags::WHITEOUT) {
                ramfs_insert_whiteout(&mut dir, &old_key)?;
            }
            dir.children.insert(new_name.clone(), inode_to_move.clone());
            inode_to_move.0.lock().name = new_name;
            return Ok(());
        }

        if self_id < target_id {
            let mut src_dir = self.0.lock();
            let mut dst_dir = target_locked.0.lock();
            ramfs_move_entry_between_dirs(&mut src_dir, &mut dst_dir, &old_key, &new_name, flags)
        } else {
            let mut dst_dir = target_locked.0.lock();
            let mut src_dir = self.0.lock();
            ramfs_move_entry_between_dirs(&mut src_dir, &mut dst_dir, &old_key, &new_name, flags)
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
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let mut inode = self.0.lock();
        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        // Regular file: delegate to create(), must drop lock first to avoid deadlock
        let file_type = FileType::from(mode);
        if unlikely(file_type == FileType::File) {
            drop(inode);
            return self.create(filename, FileType::File, mode);
        }

        let filename = DName::from(filename);

        // Determine file type from mode
        let file_type = match file_type {
            FileType::Pipe => FileType::Pipe,
            FileType::CharDevice => FileType::CharDevice,
            FileType::BlockDevice => FileType::BlockDevice,
            FileType::Socket => FileType::Socket,
            _ => return Err(SystemError::EINVAL),
        };
        let init =
            crate::filesystem::vfs::permission::child_inode_init(&inode.metadata, file_type, mode);

        let nod = Arc::new(LockedRamFSInode(Mutex::new(RamFSInode {
            parent: inode.self_ref.clone(),
            self_ref: Weak::default(),
            children: BTreeMap::new(),
            data: Vec::new(),
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
        })));

        nod.0.lock().self_ref = Arc::downgrade(&nod);

        // FIFO requires creating an actual pipe inode
        if mode.contains(InodeMode::S_IFIFO) {
            let pipe_inode = LockedPipeInode::new();
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
