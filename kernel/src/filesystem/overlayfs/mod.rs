#![allow(dead_code, unused_variables, unused_imports)]
pub mod copy_up;
pub mod entry;

use super::ramfs::{LockedRamFSInode, RamFSInode};
use super::vfs::{self, FileSystem, FileType, FsInfo, IndexNode, Metadata, SuperBlock};
use super::vfs::{FSMAKER, ROOT_INODE};
use crate::driver::base::device::device_number::DeviceNumber;
use crate::driver::base::device::device_number::Major;
use crate::filesystem::vfs::{FileSystemMaker, FileSystemMakerData};
use crate::libs::spinlock::SpinLock;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::sync::Weak;
use alloc::vec::Vec;
use entry::{OvlEntry, OvlLayer};
use linkme::distributed_slice;
use system_error::SystemError;

const WHITEOUT_MODE: u64 = 0o020000 | 0o600; // whiteout字符设备文件模式与权限
const WHITEOUT_DEV: DeviceNumber = DeviceNumber::new(Major::UNNAMED_MAJOR, 0); // Whiteout 文件设备号
const WHITEOUT_FLAG: u64 = 0x1;

#[distributed_slice(FSMAKER)]
static OVERLAYFSMAKER: FileSystemMaker = FileSystemMaker::new(
    "overlay",
    &(OverlayFS::make_overlayfs
        as fn(
            Option<&dyn FileSystemMakerData>,
        ) -> Result<Arc<dyn FileSystem + 'static>, SystemError>),
);
#[derive(Debug)]
pub struct OverlayMountData {
    upper_dir: String,
    lower_dirs: Vec<String>,
    work_dir: String,
}

impl OverlayMountData {
    pub fn from_row(raw_data: *const u8) -> Result<Self, SystemError> {
        if raw_data.is_null() {
            return Err(SystemError::EINVAL);
        }
        let len = (0..)
            .find(|&i| unsafe { raw_data.add(i).read() } == 0)
            .ok_or(SystemError::EINVAL)?;
        let slice = unsafe { core::slice::from_raw_parts(raw_data, len) };
        let raw_str = core::str::from_utf8(slice).map_err(|_| SystemError::EINVAL)?;
        let mut data = OverlayMountData {
            upper_dir: String::new(),
            lower_dirs: Vec::new(),
            work_dir: String::new(),
        };

        for pair in raw_str.split(',') {
            let mut parts = pair.split('=');
            let key = parts.next().ok_or(SystemError::EINVAL)?;
            let value = parts.next().ok_or(SystemError::EINVAL)?;

            match key {
                "upperdir" => data.upper_dir = value.into(),
                "lowerdir" => data.lower_dirs = value.split(':').map(|s| s.into()).collect(),
                "workdir" => data.work_dir = value.into(),
                _ => return Err(SystemError::EINVAL),
            }
        }
        Ok(data)
    }
}
impl FileSystemMakerData for OverlayMountData {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}
#[derive(Debug)]
pub struct OvlSuperBlock {
    super_block: SuperBlock,
    pseudo_dev: DeviceNumber, // 虚拟设备号
    is_lower: bool,
}

#[derive(Debug)]
struct OverlayFS {
    numlayer: usize,
    numfs: u32,
    numdatalayer: usize,
    layers: Vec<OvlLayer>, // 第0层为读写层，后面是只读层
    workdir: Arc<OvlInode>,
    root_inode: Arc<OvlInode>,
}

#[derive(Debug)]
pub struct OvlInode {
    redirect: String, // 重定向路径
    file_type: FileType,
    flags: SpinLock<u64>,
    upper_inode: SpinLock<Option<Arc<dyn IndexNode>>>, // 读写层
    lower_inode: Option<Arc<dyn IndexNode>>,           // 只读层
    oe: Arc<OvlEntry>,
    fs: Weak<OverlayFS>,
}
impl OvlInode {
    pub fn new(
        redirect: String,
        upper: Option<Arc<dyn IndexNode>>,
        lower_inode: Option<Arc<dyn IndexNode>>,
    ) -> Self {
        Self {
            redirect,
            file_type: FileType::Dir,
            flags: SpinLock::new(0),
            upper_inode: SpinLock::new(upper),
            lower_inode,
            oe: Arc::new(OvlEntry::new()),
            fs: Weak::default(),
        }
    }
}

impl FileSystem for OverlayFS {
    fn root_inode(&self) -> Arc<dyn IndexNode> {
        self.root_inode.clone()
    }

    fn info(&self) -> vfs::FsInfo {
        FsInfo {
            blk_dev_id: 0,
            max_name_len: 255,
        }
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn name(&self) -> &str {
        "overlayfs"
    }

    fn super_block(&self) -> SuperBlock {
        todo!()
    }
}

impl OverlayFS {
    pub fn ovl_upper_mnt(&self) -> Arc<dyn IndexNode> {
        self.layers[0].mnt.clone()
    }
    pub fn make_overlayfs(
        data: Option<&dyn FileSystemMakerData>,
    ) -> Result<Arc<dyn FileSystem + 'static>, SystemError> {
        let mount_data = data
            .and_then(|d| d.as_any().downcast_ref::<OverlayMountData>())
            .ok_or(SystemError::EINVAL)?;

        let upper_inode = ROOT_INODE()
            .lookup(&mount_data.upper_dir)
            .map_err(|_| SystemError::EINVAL)?;
        let upper_layer = OvlLayer {
            mnt: Arc::new(OvlInode::new(
                mount_data.upper_dir.clone(),
                Some(upper_inode),
                None,
            )),
            index: 0,
            fsid: 0,
        };

        let lower_layers: Result<Vec<OvlLayer>, SystemError> = mount_data
            .lower_dirs
            .iter()
            .enumerate()
            .map(|(i, dir)| {
                let lower_inode = ROOT_INODE().lookup(dir).map_err(|_| SystemError::EINVAL)?; // 处理错误
                Ok(OvlLayer {
                    mnt: Arc::new(OvlInode::new(dir.clone(), None, Some(lower_inode))),
                    index: (i + 1) as u32,
                    fsid: (i + 1) as u32,
                })
            })
            .collect();

        let lower_layers = lower_layers?;

        let workdir = Arc::new(OvlInode::new(mount_data.work_dir.clone(), None, None));

        if lower_layers.is_empty() {
            return Err(SystemError::EINVAL);
        }

        let mut layers = Vec::new();
        layers.push(upper_layer);
        layers.extend(lower_layers);

        let root_inode = layers[0].mnt.clone();

        let fs = OverlayFS {
            numlayer: layers.len(),
            numfs: 1,
            numdatalayer: layers.len() - 1,
            layers,
            workdir,
            root_inode,
        };
        Ok(Arc::new(fs))
    }
}

impl OvlInode {
    pub fn ovl_lower_redirect(&self) -> Option<&str> {
        if self.file_type == FileType::File || self.file_type == FileType::Dir {
            Some(&self.redirect)
        } else {
            None
        }
    }

    pub fn create_whiteout(&self, name: &str) -> Result<(), SystemError> {
        let whiteout_mode = vfs::syscall::ModeType::S_IFCHR;
        let mut upper_inode = self.upper_inode.lock();
        if let Some(ref upper_inode) = *upper_inode {
            upper_inode.mknod(name, whiteout_mode, WHITEOUT_DEV)?;
        } else {
            let new_inode = self
                .fs
                .upgrade()
                .ok_or(SystemError::EROFS)?
                .root_inode()
                .create(name, FileType::CharDevice, whiteout_mode)?;
            *upper_inode = Some(new_inode);
        }
        let mut flags = self.flags.lock();
        *flags |= WHITEOUT_FLAG; // 标记为 whiteout
        Ok(())
    }

    fn is_whiteout(&self) -> bool {
        let flags = self.flags.lock();
        self.file_type == FileType::CharDevice && (*flags & WHITEOUT_FLAG) != 0
    }

    fn has_whiteout(&self, name: &str) -> bool {
        let upper_inode = self.upper_inode.lock();
        if let Some(ref upper_inode) = *upper_inode {
            if let Ok(inode) = upper_inode.find(name) {
                if let Some(ovl_inode) = inode.as_any_ref().downcast_ref::<OvlInode>() {
                    return ovl_inode.is_whiteout();
                }
            }
        }
        false
    }
}

impl IndexNode for OvlInode {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        data: crate::libs::spinlock::SpinLockGuard<vfs::FilePrivateData>,
    ) -> Result<usize, system_error::SystemError> {
        if let Some(ref upper_inode) = *self.upper_inode.lock() {
            return upper_inode.read_at(offset, len, buf, data);
        }

        if let Some(lower_inode) = &self.lower_inode {
            return lower_inode.read_at(offset, len, buf, data);
        }

        Err(SystemError::ENOENT)
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        data: crate::libs::spinlock::SpinLockGuard<vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        if (*self.upper_inode.lock()).is_none() {
            self.copy_up()?;
        }
        if let Some(ref upper_inode) = *self.upper_inode.lock() {
            return upper_inode.write_at(offset, len, buf, data);
        }

        Err(SystemError::EROFS)
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        self.fs.upgrade().unwrap()
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        if let Some(ref upper_inode) = *self.upper_inode.lock() {
            return upper_inode.metadata();
        }

        if let Some(ref lower_inode) = self.lower_inode {
            return lower_inode.metadata();
        }
        Ok(Metadata::default())
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn list(&self) -> Result<Vec<String>, system_error::SystemError> {
        let mut entries: Vec<String> = Vec::new();
        let upper_inode = self.upper_inode.lock();
        if let Some(ref upper_inode) = *upper_inode {
            let upper_entries = upper_inode.list()?;
            entries.extend(upper_entries);
        }
        if let Some(lower_inode) = &self.lower_inode {
            let lower_entries = lower_inode.list()?;
            for entry in lower_entries {
                if !entries.contains(&entry) && !self.has_whiteout(&entry) {
                    entries.push(entry);
                }
            }
        }

        Ok(entries)
    }

    fn mkdir(
        &self,
        name: &str,
        mode: vfs::syscall::ModeType,
    ) -> Result<Arc<dyn IndexNode>, system_error::SystemError> {
        if let Some(ref upper_inode) = *self.upper_inode.lock() {
            upper_inode.mkdir(name, mode)
        } else {
            Err(SystemError::EROFS)
        }
    }

    fn rmdir(&self, name: &str) -> Result<(), SystemError> {
        let upper_inode = self.upper_inode.lock();
        if let Some(ref upper_inode) = *upper_inode {
            upper_inode.rmdir(name)?;
        } else if let Some(lower_inode) = &self.lower_inode {
            if lower_inode.find(name).is_ok() {
                self.create_whiteout(name)?;
            } else {
                return Err(SystemError::ENOENT);
            }
        } else {
            return Err(SystemError::ENOENT);
        }

        Ok(())
    }

    fn unlink(&self, name: &str) -> Result<(), SystemError> {
        let upper_inode = self.upper_inode.lock();
        if let Some(ref upper_inode) = *upper_inode {
            upper_inode.unlink(name)?;
        } else if let Some(lower_inode) = &self.lower_inode {
            if lower_inode.find(name).is_ok() {
                self.create_whiteout(name)?;
            } else {
                return Err(SystemError::ENOENT);
            }
        } else {
            return Err(SystemError::ENOENT);
        }

        Ok(())
    }

    fn link(
        &self,
        name: &str,
        other: &Arc<dyn IndexNode>,
    ) -> Result<(), system_error::SystemError> {
        if let Some(ref upper_inode) = *self.upper_inode.lock() {
            upper_inode.link(name, other)
        } else {
            Err(SystemError::EROFS)
        }
    }

    fn create(
        &self,
        name: &str,
        file_type: vfs::FileType,
        mode: vfs::syscall::ModeType,
    ) -> Result<Arc<dyn IndexNode>, system_error::SystemError> {
        if let Some(ref upper_inode) = *self.upper_inode.lock() {
            upper_inode.create(name, file_type, mode)
        } else {
            Err(SystemError::EROFS)
        }
    }

    fn find(&self, name: &str) -> Result<Arc<dyn IndexNode>, system_error::SystemError> {
        let upper_inode = self.upper_inode.lock();
        if let Some(ref upper) = *upper_inode {
            if let Ok(inode) = upper.find(name) {
                return Ok(inode);
            }
        }
        if self.has_whiteout(name) {
            return Err(SystemError::ENOENT);
        }

        if let Some(lower) = &self.lower_inode {
            if let Ok(inode) = lower.find(name) {
                return Ok(inode);
            }
        }

        Err(SystemError::ENOENT)
    }

    fn mknod(
        &self,
        filename: &str,
        mode: vfs::syscall::ModeType,
        dev_t: crate::driver::base::device::device_number::DeviceNumber,
    ) -> Result<Arc<dyn IndexNode>, system_error::SystemError> {
        let upper_inode = self.upper_inode.lock();
        if let Some(ref inode) = *upper_inode {
            inode.mknod(filename, mode, dev_t)
        } else {
            Err(SystemError::EROFS)
        }
    }
}
