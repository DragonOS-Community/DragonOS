use core::fmt::Debug;

use self::{dir::SysKernDirPriv, file::SysKernFilePriv};

use super::{
    kernfs::{KernFS, KernFSInode},
    vfs::{syscall::ModeType, FileSystem},
};
use crate::{
    driver::base::kobject::KObject,
    filesystem::vfs::ROOT_INODE,
    kinfo, kwarn,
    libs::{casting::DowncastArc, once::Once},
};
use alloc::sync::Arc;
use system_error::SystemError;

pub mod dir;
pub mod file;
pub mod group;
pub mod symlink;

/// 全局的sysfs实例
pub(self) static mut SYSFS_INSTANCE: Option<SysFS> = None;

#[inline(always)]
pub fn sysfs_instance() -> &'static SysFS {
    unsafe {
        return &SYSFS_INSTANCE.as_ref().unwrap();
    }
}

pub fn sysfs_init() -> Result<(), SystemError> {
    static INIT: Once = Once::new();
    let mut result = None;
    INIT.call_once(|| {
        kinfo!("Initializing SysFS...");

        // 创建 sysfs 实例
        // let sysfs: Arc<OldSysFS> = OldSysFS::new();
        let sysfs = SysFS::new();
        unsafe { SYSFS_INSTANCE = Some(sysfs) };

        // sysfs 挂载
        let _t = ROOT_INODE()
            .find("sys")
            .expect("Cannot find /sys")
            .mount(sysfs_instance().fs().clone())
            .expect("Failed to mount sysfs");
        kinfo!("SysFS mounted.");

        // kdebug!("sys_bus_init result: {:?}", SYS_BUS_INODE().list());
        result = Some(Ok(()));
    });

    return result.unwrap();
}

/// SysFS在KernFS的inode中的私有信息
#[allow(dead_code)]
#[derive(Debug)]
pub enum SysFSKernPrivateData {
    Dir(SysKernDirPriv),
    File(SysKernFilePriv),
}

impl SysFSKernPrivateData {
    #[inline(always)]
    pub fn callback_read(&self, buf: &mut [u8], offset: usize) -> Result<usize, SystemError> {
        match self {
            SysFSKernPrivateData::File(file) => {
                let len = file.callback_read(buf, offset)?;

                return Ok(len);
            }
            _ => {
                return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
            }
        }
    }

    #[inline(always)]
    pub fn callback_write(&self, buf: &[u8], offset: usize) -> Result<usize, SystemError> {
        match self {
            SysFSKernPrivateData::File(file) => {
                return file.callback_write(buf, offset);
            }
            _ => {
                return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
            }
        }
    }
}

/// sysfs文件目录的属性组
pub trait AttributeGroup: Debug + Send + Sync {
    /// 属性组的名称
    ///
    /// 如果属性组的名称为None，则所有的属性都会被添加到父目录下，而不是创建一个新的目录
    fn name(&self) -> Option<&str>;
    /// 属性组的属性列表
    fn attrs(&self) -> &[&'static dyn Attribute];

    /// 属性在当前属性组内的权限（该方法可选）
    ///
    /// 如果返回None，则使用Attribute的mode()方法返回的权限
    ///
    /// 如果返回Some，则使用返回的权限。
    /// 如果要标识属性不可见，则返回Some(ModeType::empty())
    fn is_visible(&self, kobj: Arc<dyn KObject>, attr: &'static dyn Attribute) -> Option<ModeType>;
}

/// sysfs文件的属性
pub trait Attribute: Debug + Send + Sync {
    fn name(&self) -> &str;
    fn mode(&self) -> ModeType;

    fn support(&self) -> SysFSOpsSupport;

    fn show(&self, _kobj: Arc<dyn KObject>, _buf: &mut [u8]) -> Result<usize, SystemError> {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    fn store(&self, _kobj: Arc<dyn KObject>, _buf: &[u8]) -> Result<usize, SystemError> {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
}

pub trait BinAttribute: Attribute {
    fn support_battr(&self) -> SysFSOpsSupport;

    fn write(
        &self,
        _kobj: Arc<dyn KObject>,
        _buf: &[u8],
        _offset: usize,
    ) -> Result<usize, SystemError> {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    fn read(
        &self,
        _kobj: Arc<dyn KObject>,
        _buf: &mut [u8],
        _offset: usize,
    ) -> Result<usize, SystemError> {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    fn size(&self) -> usize;
}

pub trait SysFSOps: Debug {
    /// 获取当前文件的支持的操作
    fn support(&self, attr: &dyn Attribute) -> SysFSOpsSupport {
        return attr.support();
    }

    fn support_battr(&self, attr: &Arc<dyn BinAttribute>) -> SysFSOpsSupport {
        return attr.support();
    }

    fn show(
        &self,
        kobj: Arc<dyn KObject>,
        attr: &dyn Attribute,
        buf: &mut [u8],
    ) -> Result<usize, SystemError>;

    fn store(
        &self,
        kobj: Arc<dyn KObject>,
        attr: &dyn Attribute,
        buf: &[u8],
    ) -> Result<usize, SystemError>;
}

bitflags! {
    pub struct SysFSOpsSupport: u8{
        // === for attribute ===
        const SHOW = 1 << 0;
        const STORE = 1 << 1;
        // === for bin attribute ===
        const READ = 1 << 2;
        const WRITE = 1 << 3;
    }
}

#[derive(Debug)]
pub struct SysFS {
    root_inode: Arc<KernFSInode>,
    kernfs: Arc<KernFS>,
}

impl SysFS {
    pub fn new() -> Self {
        let kernfs: Arc<KernFS> = KernFS::new();

        let root_inode: Arc<KernFSInode> = kernfs.root_inode().downcast_arc().unwrap();

        let sysfs = SysFS { root_inode, kernfs };

        return sysfs;
    }

    pub fn root_inode(&self) -> &Arc<KernFSInode> {
        return &self.root_inode;
    }

    pub fn fs(&self) -> &Arc<KernFS> {
        return &self.kernfs;
    }

    /// 警告：重复的sysfs entry
    pub(self) fn warn_duplicate(&self, parent: &Arc<KernFSInode>, name: &str) {
        let path = self.kernfs_path(parent);
        kwarn!("duplicate sysfs entry: {path}/{name}");
    }
}
