use core::sync::atomic::{AtomicU32, Ordering};

use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use ida::IdAllocator;
use system_error::SystemError;
use unified_init::macros::unified_init;

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
    filesystem::vfs::{syscall::ModeType, FileType, ROOT_INODE},
    init::initcall::INITCALL_FS,
    libs::spinlock::{SpinLock, SpinLockGuard},
    time::TimeSpec,
};

use super::vfs::{
    core::generate_inode_id, FilePrivateData, FileSystem, FsInfo, IndexNode, Metadata,
};

const DEV_PTYFS_MAX_NAMELEN: usize = 16;

#[allow(dead_code)]
const PTY_NR_LIMIT: usize = 4096;

#[derive(Debug)]
pub struct DevPtsFs {
    /// 根节点
    root_inode: Arc<LockedDevPtsFSInode>,
    pts_ida: IdAllocator,
    pts_count: AtomicU32,
}

impl DevPtsFs {
    pub fn new() -> Arc<Self> {
        let root_inode = Arc::new(LockedDevPtsFSInode::new());
        let ret = Arc::new(Self {
            root_inode,
            pts_ida: IdAllocator::new(1, NR_UNIX98_PTY_MAX as usize),
            pts_count: AtomicU32::new(0),
        });

        ret.root_inode.set_fs(Arc::downgrade(&ret));

        ret
    }

    pub fn alloc_index(&self) -> Result<usize, SystemError> {
        self.pts_ida.alloc().ok_or(SystemError::ENOSPC)
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
                metadata: Metadata {
                    dev_id: 0,
                    inode_id: generate_inode_id(),
                    size: 0,
                    blk_size: 0,
                    blocks: 0,
                    atime: TimeSpec::default(),
                    mtime: TimeSpec::default(),
                    ctime: TimeSpec::default(),
                    file_type: FileType::Dir,
                    mode: ModeType::from_bits_truncate(0x777),
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
        _mode: &super::vfs::file::FileMode,
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
        todo!()
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
        _mode: super::vfs::syscall::ModeType,
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

        metadata.mode.insert(ModeType::S_IFCHR);
        metadata.raw_dev =
            DeviceNumber::new(Major::UNIX98_PTY_SLAVE_MAJOR, name.parse::<u32>().unwrap());

        result.set_metadata(&metadata)?;

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

#[unified_init(INITCALL_FS)]
#[inline(never)]
pub fn devpts_init() -> Result<(), SystemError> {
    let dev_inode = ROOT_INODE().find("dev")?;

    let pts_inode = dev_inode.create("pts", FileType::Dir, ModeType::from_bits_truncate(0o755))?;

    // 创建 devptsfs 实例
    let ptsfs: Arc<DevPtsFs> = DevPtsFs::new();

    // let mountfs = dev_inode.mount(ptsfs).expect("Failed to mount DevPtsFS");

    pts_inode.mount(ptsfs).expect("Failed to mount DevPtsFS");
    kinfo!("DevPtsFs mounted.");

    Ok(())
}
