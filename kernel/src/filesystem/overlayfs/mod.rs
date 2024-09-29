pub mod copy_up;
pub mod entry;
use super::vfs::{self, FileSystem, FileType, FsInfo, IndexNode, SuperBlock};
use crate::libs::spinlock::SpinLock;
use crate::{include::bindings::bindings::dev_t, libs::mutex::Mutex};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::sync::Weak;
use alloc::vec::Vec;
use entry::{OvlEntry, OvlLayer};
use system_error::SystemError;

const WHITEOUT_MODE: u64 = 0o020000 | 0o600; // whiteout字符设备文件模式与权限
const WHITEOUT_DEV: dev_t = 0; // Whiteout 文件设备号
const WHITEOUT_FLAG: u64 = 0x1;

#[derive(Debug)]
pub struct OvlSuperBlock {
    super_block: SuperBlock,
    pseudo_dev: dev_t, // 虚拟设备号
    is_lower: bool,
}

#[derive(Debug)]
struct OverlayFS {
    numlayer: u32,
    numfs: u32,
    numdatalayer: u32,
    layers: Vec<OvlLayer>, // 第0层为读写层，后面是只读层
    workbasedir: Arc<dyn IndexNode>,
    workdir: Arc<dyn IndexNode>,
    root_inode: Arc<dyn IndexNode>,
}

#[derive(Debug)]
struct OvlInode {
    redirect: String, // 重定向路径
    file_type: FileType,
    flags: SpinLock<u64>,
    upper_inode: SpinLock<Option<Arc<dyn IndexNode>>>, // 读写层
    lower_inode: Option<Arc<dyn IndexNode>>,           // 只读层
    oe: Arc<OvlEntry>,
    fs: Weak<OverlayFS>,
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
}

impl OvlInode {
    // 返回常规文件相关联的重定向路径
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
            upper_inode.mknod(name, whiteout_mode, WHITEOUT_DEV.into())?;
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
        } else {
            if let Some(lower_inode) = &self.lower_inode {
                if lower_inode.find(name).is_ok() {
                    self.create_whiteout(name)?;
                } else {
                    return Err(SystemError::ENOENT);
                }
            } else {
                return Err(SystemError::ENOENT);
            }
        }

        Ok(())
    }

    fn unlink(&self, name: &str) -> Result<(), SystemError> {
        let upper_inode = self.upper_inode.lock();
        if let Some(ref upper_inode) = *upper_inode {
            upper_inode.unlink(name)?;
        } else {
            if let Some(lower_inode) = &self.lower_inode {
                if lower_inode.find(name).is_ok() {
                    self.create_whiteout(name)?;
                } else {
                    return Err(SystemError::ENOENT);
                }
            } else {
                return Err(SystemError::ENOENT);
            }
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
