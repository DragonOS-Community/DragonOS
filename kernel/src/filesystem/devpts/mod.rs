use core::sync::atomic::{AtomicU32, Ordering};

use crate::{
    driver::{
        base::device::{
            device_number::{DeviceNumber, Major},
            IdTable,
        },
        tty::{
            pty::unix98pty::NR_UNIX98_PTY_MAX,
            tty_device::{PtyType, TtyDevice, TtyType},
        },
    },
    filesystem::vfs::{
        mount::{do_mount_mkdir, MountFlags},
        FileType, InodeMode,
    },
    libs::spinlock::{SpinLock, SpinLockGuard},
    time::PosixTimeSpec,
};
use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use ida::IdAllocator;
use log::info;
use system_error::SystemError;

use super::{
    devfs::DeviceINode,
    vfs::{vcore::generate_inode_id, FilePrivateData, FileSystem, FsInfo, IndexNode, InodeFlags, Metadata},
};

const DEV_PTYFS_MAX_NAMELEN: usize = 16;

#[allow(dead_code)]
const PTY_NR_LIMIT: usize = 4096;

#[derive(Debug)]
pub struct DevPtsFs {
    /// 根节点
    root_inode: Arc<LockedDevPtsFSInode>,
    pts_ida: SpinLock<IdAllocator>,
    pts_count: AtomicU32,
}

impl DevPtsFs {
    pub fn new() -> Arc<Self> {
        let root_inode = Arc::new(LockedDevPtsFSInode::new());
        root_inode.inner.lock().parent = Arc::downgrade(&root_inode);
        root_inode.inner.lock().self_ref = Arc::downgrade(&root_inode);
        let ret = Arc::new(Self {
            root_inode,
            pts_ida: SpinLock::new(IdAllocator::new(0, NR_UNIX98_PTY_MAX as usize).unwrap()),
            pts_count: AtomicU32::new(0),
        });

        ret.root_inode.set_fs(Arc::downgrade(&ret));

        ret
    }

    pub fn alloc_index(&self) -> Result<usize, SystemError> {
        self.pts_ida.lock().alloc().ok_or(SystemError::ENOSPC)
    }
}

impl FileSystem for DevPtsFs {
    fn root_inode(&self) -> Arc<dyn IndexNode> {
        self.root_inode.clone()
    }

    fn info(&self) -> super::vfs::FsInfo {
        return FsInfo {
            blk_dev_id: 0,
            max_name_len: DEV_PTYFS_MAX_NAMELEN,
        };
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn name(&self) -> &str {
        "devpts"
    }

    fn super_block(&self) -> super::vfs::SuperBlock {
        todo!()
    }
}

#[derive(Debug)]
pub struct LockedDevPtsFSInode {
    inner: SpinLock<PtsDevInode>,
}

impl LockedDevPtsFSInode {
    pub fn new() -> Self {
        Self {
            inner: SpinLock::new(PtsDevInode {
                fs: Weak::new(),
                children: Some(BTreeMap::new()),
                parent: Weak::new(),
                self_ref: Weak::new(),
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
                    mode: InodeMode::from_bits_truncate(0o777),
                    flags: InodeFlags::empty(),
                    nlinks: 1,
                    uid: 0,
                    gid: 0,
                    raw_dev: DeviceNumber::default(),
                },
            }),
        }
    }

    pub fn set_fs(&self, fs: Weak<DevPtsFs>) {
        self.inner.lock().fs = fs;
    }
}

#[derive(Debug)]
pub struct PtsDevInode {
    fs: Weak<DevPtsFs>,
    children: Option<BTreeMap<String, Arc<TtyDevice>>>,
    metadata: Metadata,
    parent: Weak<LockedDevPtsFSInode>,
    self_ref: Weak<LockedDevPtsFSInode>,
}

impl PtsDevInode {
    pub fn children_unchecked(&self) -> &BTreeMap<String, Arc<TtyDevice>> {
        self.children.as_ref().unwrap()
    }

    pub fn children_unchecked_mut(&mut self) -> &mut BTreeMap<String, Arc<TtyDevice>> {
        self.children.as_mut().unwrap()
    }
}

impl IndexNode for LockedDevPtsFSInode {
    fn open(
        &self,
        _data: SpinLockGuard<FilePrivateData>,
        _mode: &super::vfs::file::FileFlags,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    fn metadata(&self) -> Result<super::vfs::Metadata, SystemError> {
        let inode = self.inner.lock();
        let metadata = inode.metadata.clone();

        return Ok(metadata);
    }

    fn close(&self, _data: SpinLockGuard<FilePrivateData>) -> Result<(), SystemError> {
        // TODO: 回收
        Ok(())
    }

    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &mut [u8],
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, system_error::SystemError> {
        todo!()
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, system_error::SystemError> {
        todo!()
    }

    fn fs(&self) -> alloc::sync::Arc<dyn super::vfs::FileSystem> {
        self.inner.lock().fs.upgrade().unwrap()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn list(&self) -> Result<alloc::vec::Vec<alloc::string::String>, system_error::SystemError> {
        let info = self.metadata()?;
        if info.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        let mut keys: Vec<String> = Vec::new();
        keys.push(String::from("."));
        keys.push(String::from(".."));
        keys.append(
            &mut self
                .inner
                .lock()
                .children_unchecked()
                .keys()
                .cloned()
                .collect(),
        );

        return Ok(keys);
    }

    fn create_with_data(
        &self,
        name: &str,
        file_type: FileType,
        _mode: super::vfs::InodeMode,
        _data: usize,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        if file_type != FileType::CharDevice {
            return Err(SystemError::ENOSYS);
        }

        let mut guard = self.inner.lock();

        if guard.children_unchecked_mut().contains_key(name) {
            return Err(SystemError::EEXIST);
        }

        let fs = guard.fs.upgrade().unwrap();

        let result = TtyDevice::new(
            name.to_string(),
            IdTable::new(name.to_string(), None),
            TtyType::Pty(PtyType::Pts),
        );

        let mut metadata = result.metadata()?;

        metadata.mode.insert(InodeMode::S_IFCHR);
        metadata.raw_dev =
            DeviceNumber::new(Major::UNIX98_PTY_SLAVE_MAJOR, name.parse::<u32>().unwrap());

        result.set_metadata(&metadata)?;

        result.set_devpts_fs(Arc::downgrade(&fs));
        result.set_devpts_parent(guard.self_ref.clone());

        guard
            .children_unchecked_mut()
            .insert(name.to_string(), result.clone());

        fs.pts_count.fetch_add(1, Ordering::SeqCst);

        Ok(result)
    }

    fn find(&self, name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        let guard = self.inner.lock();

        if let Some(dev) = guard.children_unchecked().get(name) {
            Ok(dev.clone() as Arc<dyn IndexNode>)
        } else {
            Err(SystemError::ENOENT)
        }
    }

    fn unlink(&self, name: &str) -> Result<(), SystemError> {
        let mut guard = self.inner.lock();
        guard.children_unchecked_mut().remove(name);
        Ok(())
    }
}

pub fn devpts_init() -> Result<(), SystemError> {
    // 创建 devptsfs 实例
    let ptsfs: Arc<DevPtsFs> = DevPtsFs::new();

    do_mount_mkdir(ptsfs, "/dev/pts", MountFlags::empty()).expect("Failed to mount DevPtsFS");
    info!("DevPtsFs mounted.");

    Ok(())
}
