use core::{
    any::Any,
    sync::atomic::{AtomicU32, Ordering},
};

use crate::libs::mutex::MutexGuard;
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
        FileSystem, FileType, FsInfo, InodeMode, MountableFileSystem, SuperBlock, FSMAKER,
    },
    libs::spinlock::SpinLock,
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
    vfs::{vcore::generate_inode_id, FilePrivateData, IndexNode, InodeFlags, Magic, Metadata},
};
use crate::{filesystem::vfs::FileSystemMakerData, register_mountable_fs};
use linkme::distributed_slice;

const DEV_PTYFS_MAX_NAMELEN: usize = 16;

#[allow(dead_code)]
const PTY_NR_LIMIT: usize = 4096;

#[derive(Debug, Clone)]
struct DevPtsOptions {
    root_mode: InodeMode,
    pts_mode: InodeMode,
    ptmx_mode: InodeMode,
    new_instance: bool,
}

impl Default for DevPtsOptions {
    fn default() -> Self {
        Self {
            root_mode: InodeMode::from_bits_truncate(0o755),
            pts_mode: InodeMode::from_bits_truncate(0o620),
            ptmx_mode: InodeMode::from_bits_truncate(0o666),
            new_instance: false,
        }
    }
}

impl FileSystemMakerData for DevPtsOptions {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
pub struct DevPtsFs {
    /// 根节点
    root_inode: Arc<LockedDevPtsFSInode>,
    pts_ida: SpinLock<IdAllocator>,
    pts_count: AtomicU32,
    opts: DevPtsOptions,
}

impl DevPtsFs {
    fn new_with_opts(opts: DevPtsOptions) -> Arc<Self> {
        let root_inode = Arc::new(LockedDevPtsFSInode::new());
        root_inode.inner.lock().parent = Arc::downgrade(&root_inode);
        root_inode.inner.lock().self_ref = Arc::downgrade(&root_inode);
        {
            // 设置根目录的权限与块大小
            let md = &mut root_inode.inner.lock().metadata;
            md.mode = opts.root_mode | InodeMode::S_IFDIR;
            md.blk_size = 1024;
        }
        let ret = Arc::new(Self {
            root_inode,
            pts_ida: SpinLock::new(IdAllocator::new(0, NR_UNIX98_PTY_MAX as usize).unwrap()),
            pts_count: AtomicU32::new(0),
            opts,
        });

        ret.root_inode.set_fs(Arc::downgrade(&ret));
        ret.install_ptmx_node();

        ret
    }

    pub fn alloc_index(&self) -> Result<usize, SystemError> {
        self.pts_ida.lock().alloc().ok_or(SystemError::ENOSPC)
    }

    pub fn free_index(&self, idx: usize) {
        self.pts_ida.lock().free(idx);
        self.pts_count.fetch_sub(1, Ordering::SeqCst);
    }

    fn install_ptmx_node(&self) {
        // devpts 内部的 ptmx 节点，供 newinstance 使用。
        // 只创建一次，若已存在则忽略。
        let mut guard = self.root_inode.inner.lock();
        if guard.children_unchecked().contains_key("ptmx") {
            return;
        }

        let dev_num = DeviceNumber::new(Major::TTYAUX_MAJOR, 2);
        let ptmx_dev = TtyDevice::new(
            "ptmx".to_string(),
            IdTable::new("ptmx".to_string(), Some(dev_num)),
            TtyType::Pty(PtyType::Ptm),
        );
        let mut md = ptmx_dev.metadata().unwrap();
        md.mode = self.opts.ptmx_mode | InodeMode::S_IFCHR;
        md.raw_dev = dev_num;
        md.blk_size = 1024;
        let _ = ptmx_dev.set_metadata(&md);
        if let Some(fs_arc) = guard.fs.upgrade() {
            ptmx_dev.set_devpts_fs(Arc::downgrade(&fs_arc));
        }
        ptmx_dev.set_devpts_parent(guard.self_ref.clone());

        guard
            .children_unchecked_mut()
            .insert("ptmx".to_string(), ptmx_dev);
    }

    #[allow(dead_code)]
    pub fn is_new_instance(&self) -> bool {
        self.opts.new_instance
    }
}

impl FileSystem for DevPtsFs {
    fn root_inode(&self) -> Arc<dyn IndexNode> {
        self.root_inode.clone()
    }

    fn info(&self) -> FsInfo {
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
        SuperBlock::new(Magic::DEVPTS_MAGIC, 1024, DEV_PTYFS_MAX_NAMELEN as u64)
    }
}

impl MountableFileSystem for DevPtsFs {
    fn make_mount_data(
        raw_data: Option<&str>,
        _source: &str,
    ) -> Result<Option<Arc<dyn FileSystemMakerData + 'static>>, SystemError> {
        // 对于 devpts，挂载数据只是解析选项，将结果以 Box<DevPtsOptions> 形式传递
        let opts = parse_devpts_options(raw_data);
        Ok(Some(Arc::new(opts)))
    }

    fn make_fs(
        data: Option<&dyn FileSystemMakerData>,
    ) -> Result<Arc<dyn FileSystem + 'static>, SystemError> {
        let opts = data
            .and_then(|d| d.as_any().downcast_ref::<DevPtsOptions>())
            .cloned()
            .unwrap_or_default();
        Ok(Self::new_with_opts(opts))
    }
}

register_mountable_fs!(DevPtsFs, DEVPTS_MAKER, "devpts");

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
                    blk_size: 1024,
                    blocks: 0,
                    atime: PosixTimeSpec::default(),
                    mtime: PosixTimeSpec::default(),
                    ctime: PosixTimeSpec::default(),
                    btime: PosixTimeSpec::default(),
                    file_type: FileType::Dir,
                    mode: InodeMode::S_IRWXUGO,
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
        _data: MutexGuard<FilePrivateData>,
        _mode: &super::vfs::file::FileFlags,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    fn metadata(&self) -> Result<super::vfs::Metadata, SystemError> {
        let inode = self.inner.lock();
        let metadata = inode.metadata.clone();

        return Ok(metadata);
    }

    fn close(&self, _data: MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        // TODO: 回收
        Ok(())
    }

    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, system_error::SystemError> {
        todo!()
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
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

        metadata.mode = fs.opts.pts_mode | InodeMode::S_IFCHR;
        metadata.raw_dev =
            DeviceNumber::new(Major::UNIX98_PTY_SLAVE_MAJOR, name.parse::<u32>().unwrap());
        metadata.blk_size = 1024;

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
    let ptsfs: Arc<DevPtsFs> = DevPtsFs::new_with_opts(DevPtsOptions::default());

    do_mount_mkdir(ptsfs, "/dev/pts", MountFlags::empty()).expect("Failed to mount DevPtsFS");
    info!("DevPtsFs mounted.");

    Ok(())
}

fn parse_devpts_options(raw: Option<&str>) -> DevPtsOptions {
    let mut opts = DevPtsOptions::default();
    if let Some(raw) = raw {
        for item in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if let Some(val) = item.strip_prefix("mode=") {
                if let Ok(bits) = u32::from_str_radix(val, 8) {
                    let mode = InodeMode::from_bits_truncate(bits);
                    opts.pts_mode = mode;
                    opts.root_mode = mode;
                }
            } else if let Some(val) = item.strip_prefix("ptmxmode=") {
                if let Ok(bits) = u32::from_str_radix(val, 8) {
                    opts.ptmx_mode = InodeMode::from_bits_truncate(bits);
                }
            } else if item == "newinstance" {
                opts.new_instance = true;
            }
        }
    }
    opts
}
