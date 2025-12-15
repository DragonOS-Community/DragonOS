use core::any::Any;
use core::intrinsics::unlikely;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::filesystem::vfs::syscall::RenameFlags;
use crate::filesystem::vfs::{FileSystemMakerData, FSMAKER};
use crate::libs::rwlock::RwLock;
use crate::mm::fault::PageFaultHandler;
use crate::register_mountable_fs;
use crate::{
    driver::base::device::device_number::DeviceNumber,
    filesystem::vfs::{vcore::generate_inode_id, FileType},
    ipc::pipe::LockedPipeInode,
    libs::casting::DowncastArc,
    libs::spinlock::{SpinLock, SpinLockGuard},
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

#[derive(Debug)]
pub struct LockedTmpfsInode(pub SpinLock<TmpfsInode>);

#[derive(Debug)]
pub struct Tmpfs {
    root_inode: Arc<LockedTmpfsInode>,
    super_block: RwLock<SuperBlock>,
    size_limit: Option<u64>,
    current_size: AtomicU64,
}

#[derive(Debug)]
pub struct TmpfsInode {
    parent: Weak<LockedTmpfsInode>,
    self_ref: Weak<LockedTmpfsInode>,
    children: BTreeMap<DName, Arc<LockedTmpfsInode>>,
    data: Vec<u8>,
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
            for opt in raw.split(',').filter(|s| !s.is_empty()) {
                if let Some(v) = opt.strip_prefix("mode=") {
                    let parsed = u32::from_str_radix(v, 8).map_err(|_| SystemError::EINVAL)?;
                    mode = InodeMode::from_bits_truncate(parsed);
                } else if let Some(v) = opt.strip_prefix("size=") {
                    let (num_str, mul) = if let Some(s) = v.strip_suffix('G') {
                        (s, 1u64 << 30)
                    } else if let Some(s) = v.strip_suffix('M') {
                        (s, 1u64 << 20)
                    } else if let Some(s) = v.strip_suffix('K') {
                        (s, 1u64 << 10)
                    } else {
                        (v, 1u64)
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
}

impl Tmpfs {
    pub fn new(mount_data: &TmpfsMountData) -> Arc<Self> {
        let mut sb = SuperBlock::new(
            Magic::TMPFS_MAGIC,
            TMPFS_BLOCK_SIZE,
            TMPFS_MAX_NAMELEN as u64,
        );
        if let Some(size) = mount_data.size_bytes {
            let blocks = size / TMPFS_BLOCK_SIZE;
            sb.blocks = blocks;
            sb.bfree = blocks;
            sb.bavail = blocks;
        }

        let root: Arc<LockedTmpfsInode> =
            Arc::new(LockedTmpfsInode(SpinLock::new(TmpfsInode::new())));

        let result: Arc<Tmpfs> = Arc::new(Tmpfs {
            root_inode: root,
            super_block: RwLock::new(sb),
            size_limit: mount_data.size_bytes,
            current_size: AtomicU64::new(0),
        });

        let mut root_guard: SpinLockGuard<TmpfsInode> = result.root_inode.0.lock();
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
        if self.size_limit.is_some() {
            let size_to_decrease = size as u64;
            // 使用fetch_sub原子地减少大小
            self.current_size
                .fetch_sub(size_to_decrease, Ordering::Release);
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
        let mut inode = self.0.lock();
        if inode.metadata.file_type == FileType::Dir {
            return Err(SystemError::EINVAL);
        }
        let old_size = inode.data.len();
        if old_size > len {
            // 执行truncate
            inode.data.resize(len, 0);
            // 如果缩小，减少current_size
            let fs = inode.fs.upgrade().ok_or(SystemError::EIO)?;
            let tmpfs = fs
                .as_any_ref()
                .downcast_ref::<Tmpfs>()
                .ok_or(SystemError::EIO)?;
            tmpfs.decrease_size(old_size - len);
        }
        Ok(())
    }

    fn close(&self, _data: SpinLockGuard<FilePrivateData>) -> Result<(), SystemError> {
        Ok(())
    }

    fn open(
        &self,
        _data: SpinLockGuard<FilePrivateData>,
        _mode: &super::vfs::file::FileFlags,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        if buf.len() < len {
            return Err(SystemError::EINVAL);
        }
        let inode = self.0.lock();
        if inode.metadata.file_type == FileType::Dir {
            return Err(SystemError::EISDIR);
        }

        let start = inode.data.len().min(offset);
        let end = inode.data.len().min(offset + len);
        let src = &inode.data[start..end];
        buf[0..src.len()].copy_from_slice(src);
        Ok(src.len())
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        if buf.len() < len {
            return Err(SystemError::EINVAL);
        }
        let mut inode = self.0.lock();
        if inode.metadata.file_type == FileType::Dir {
            return Err(SystemError::EISDIR);
        }

        // 计算大小变化
        let old_size = inode.data.len();
        let new_size = (offset + len).max(old_size);
        let size_diff = new_size.saturating_sub(old_size) as u64;

        // 获取文件系统引用
        let fs = inode.fs.upgrade().ok_or(SystemError::EIO)?;
        let tmpfs = fs
            .as_any_ref()
            .downcast_ref::<Tmpfs>()
            .ok_or(SystemError::EIO)?;

        // 原子地预留空间（如果超过限制则失败）
        // 必须在持有锁的情况下进行，确保原子性
        if size_diff > 0 {
            tmpfs.increase_size(size_diff)?;
        }

        // 执行实际写入（持有锁，所以不会失败）
        let data: &mut Vec<u8> = &mut inode.data;
        if offset + len > data.len() {
            data.resize(offset + len, 0);
        }
        let target = &mut data[offset..offset + len];
        target.copy_from_slice(&buf[0..len]);

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
        let mut metadata = inode.metadata.clone();
        metadata.size = inode.data.len() as i64;
        Ok(metadata)
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
            let old_size = inode.data.len();
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

            // 执行实际resize
            inode.data.resize(len, 0);

            // 如果缩小，减少current_size
            if size_diff < 0 {
                tmpfs.decrease_size((-size_diff) as usize);
            }

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

        let result: Arc<LockedTmpfsInode> = Arc::new(LockedTmpfsInode(SpinLock::new(TmpfsInode {
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
        let mut inode: SpinLockGuard<TmpfsInode> = self.0.lock();
        let mut other_locked: SpinLockGuard<TmpfsInode> = other.0.lock();

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
        let mut inode: SpinLockGuard<TmpfsInode> = self.0.lock();
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
        let file_size = deleted_inode.data.len();
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
        let name = DName::from(name);
        let mut inode: SpinLockGuard<TmpfsInode> = self.0.lock();
        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }
        let to_delete = inode.children.get(&name).ok_or(SystemError::ENOENT)?;
        let deleted_inode = to_delete.0.lock();
        if deleted_inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        // 目录的大小通常是0（不包含数据），但为了完整性，我们也处理
        let dir_size = deleted_inode.data.len();
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
        let inode_to_move = self
            .find(old_name)?
            .downcast_arc::<LockedTmpfsInode>()
            .ok_or(SystemError::EINVAL)?;

        let new_name = DName::from(new_name);

        inode_to_move.0.lock().name = new_name.clone();

        let target_id = target.metadata()?.inode_id;

        let mut self_inode = self.0.lock();
        if target_id == self_inode.metadata.inode_id {
            if flags.contains(RenameFlags::NOREPLACE) && self_inode.children.contains_key(&new_name)
            {
                return Err(SystemError::EEXIST);
            }
            self_inode.children.remove(&DName::from(old_name));
            self_inode.children.insert(new_name, inode_to_move);
            return Ok(());
        }
        drop(self_inode);

        inode_to_move.0.lock().parent = Arc::downgrade(
            &target
                .clone()
                .downcast_arc::<LockedTmpfsInode>()
                .ok_or(SystemError::EINVAL)?,
        );

        target.link(new_name.as_ref(), &(inode_to_move as Arc<dyn IndexNode>))?;

        if let Err(e) = self.unlink(old_name) {
            target.unlink(new_name.as_ref())?;
            return Err(e);
        }

        Ok(())
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
        let inode: SpinLockGuard<TmpfsInode> = self.0.lock();
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
            return self.create(filename, FileType::File, mode);
        }

        let filename = DName::from(filename);

        let nod = Arc::new(LockedTmpfsInode(SpinLock::new(TmpfsInode {
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
}
