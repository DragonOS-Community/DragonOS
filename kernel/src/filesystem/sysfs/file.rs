use core::{intrinsics::unlikely, ops::BitAnd};

use alloc::{
    string::ToString,
    sync::{Arc, Weak},
};
use log::warn;
use system_error::SystemError;

use crate::{
    driver::base::kobject::KObject,
    filesystem::{
        kernfs::{
            callback::{KernCallbackData, KernFSCallback, KernInodePrivateData},
            KernFSInode,
        },
        sysfs::{SysFSOps, SysFSOpsSupport},
        vfs::{syscall::ModeType, PollStatus},
    },
};

use super::{Attribute, BinAttribute, SysFS, SysFSKernPrivateData};

#[derive(Debug)]
pub struct SysKernFilePriv {
    attribute: Option<&'static dyn Attribute>,
    /// bin attribute和attribute二选一，只能有一个为Some
    bin_attribute: Option<Arc<dyn BinAttribute>>,
    /// 当前文件对应的kobject
    kobj: Weak<dyn KObject>,
}

impl SysKernFilePriv {
    pub fn new(
        kobj: &Arc<dyn KObject>,
        attribute: Option<&'static dyn Attribute>,
        bin_attribute: Option<Arc<dyn BinAttribute>>,
    ) -> Self {
        if attribute.is_none() && bin_attribute.is_none() {
            panic!("attribute and bin_attribute can't be both None");
        }
        if attribute.is_some() && bin_attribute.is_some() {
            panic!("attribute and bin_attribute can't be both Some");
        }

        let kobj = Arc::downgrade(kobj);
        return Self {
            kobj,
            attribute,
            bin_attribute,
        };
    }

    #[allow(dead_code)]
    #[inline]
    pub fn attribute(&self) -> Option<&'static dyn Attribute> {
        self.attribute
    }

    pub fn callback_read(&self, buf: &mut [u8], offset: usize) -> Result<usize, SystemError> {
        if let Some(attribute) = self.attribute {
            // 当前文件所指向的kobject已经被释放
            let kobj = self.kobj.upgrade().expect("kobj is None");
            let len = attribute.show(kobj, buf)?;
            if offset > 0 {
                if len <= offset {
                    return Ok(0);
                }
                let len = len - offset;
                buf.copy_within(offset..offset + len, 0);
                buf[len] = 0;
            }
            return Ok(len);
        } else if let Some(bin_attribute) = self.bin_attribute.as_ref() {
            // 当前文件所指向的kobject已经被释放
            let kobj = self.kobj.upgrade().expect("kobj is None");
            return bin_attribute.read(kobj, buf, offset);
        } else {
            panic!("attribute and bin_attribute can't be both None");
        }
    }

    pub fn callback_write(&self, buf: &[u8], offset: usize) -> Result<usize, SystemError> {
        if let Some(attribute) = self.attribute {
            // 当前文件所指向的kobject已经被释放
            let kobj = self.kobj.upgrade().expect("kobj is None");
            return attribute.store(kobj, buf);
        } else if let Some(bin_attribute) = self.bin_attribute.as_ref() {
            // 当前文件所指向的kobject已经被释放
            let kobj = self.kobj.upgrade().expect("kobj is None");
            return bin_attribute.write(kobj, buf, offset);
        } else {
            panic!("attribute and bin_attribute can't be both None");
        }
    }
}

impl SysFS {
    /// 为指定的kobject创建一个属性文件
    ///
    /// ## 参数
    ///
    /// - `kobj` 要创建属性文件的kobject
    /// - `attr` 属性
    pub fn create_file(
        &self,
        kobj: &Arc<dyn KObject>,
        attr: &'static dyn Attribute,
    ) -> Result<(), SystemError> {
        let inode = kobj.inode().ok_or(SystemError::EINVAL)?;
        return self.add_file_with_mode(&inode, attr, attr.mode());
    }

    // https://code.dragonos.org.cn/xref/linux-6.1.9/fs/sysfs/file.c?fi=sysfs_add_file_mode_ns#271
    pub(super) fn add_file_with_mode(
        &self,
        parent: &Arc<KernFSInode>,
        attr: &'static dyn Attribute,
        mode: ModeType,
    ) -> Result<(), SystemError> {
        let x = parent.private_data_mut();
        let kobj: Arc<dyn KObject>;
        if let Some(KernInodePrivateData::SysFS(SysFSKernPrivateData::Dir(dt))) = x.as_ref() {
            kobj = dt.kobj().unwrap();
        } else {
            drop(x);
            let path = self.kernfs_path(parent);
            panic!("parent '{path}' is not a dir");
        }
        drop(x);

        let sysfs_ops: &dyn SysFSOps = kobj.kobj_type().unwrap().sysfs_ops().ok_or_else(|| {
            warn!("missing sysfs attribute operations for kobject: {kobj:?}");
            SystemError::EINVAL
        })?;

        // assume that all sysfs ops are preallocated.

        let sys_support = sysfs_ops.support(attr);

        let kern_callback: &'static dyn KernFSCallback;
        if sys_support.contains(SysFSOpsSupport::ATTR_SHOW)
            && sys_support.contains(SysFSOpsSupport::ATTR_STORE)
        {
            kern_callback = &PreallocKFOpsRW;
        } else if sys_support.contains(SysFSOpsSupport::ATTR_SHOW) {
            kern_callback = &PreallocKFOpsReadOnly;
        } else if sys_support.contains(SysFSOpsSupport::ATTR_STORE) {
            kern_callback = &PreallocKFOpsWriteOnly;
        } else {
            kern_callback = &PreallocKFOpsEmpty;
        }

        let sys_priv = SysFSKernPrivateData::File(SysKernFilePriv::new(&kobj, Some(attr), None));
        let r = parent.add_file(
            attr.name().to_string(),
            mode.bitand(ModeType::from_bits_truncate(0o777)),
            Some(4096),
            Some(KernInodePrivateData::SysFS(sys_priv)),
            Some(kern_callback),
        );

        if let Err(e) = r {
            if e == SystemError::EEXIST {
                self.warn_duplicate(parent, attr.name());
            }

            return Err(e);
        }
        return Ok(());
    }

    /// 在sysfs中删除某个kobject的属性文件
    ///
    /// 如果属性文件不存在，则发出一个警告
    ///
    /// ## 参数
    ///
    /// - `kobj` 要删除属性文件的kobject
    /// - `attr` 属性
    pub fn remove_file(&self, kobj: &Arc<dyn KObject>, attr: &'static dyn Attribute) {
        let parent = kobj.inode();

        if let Some(parent) = parent {
            let r = parent.remove(attr.name());
            if unlikely(r.is_err()) {
                warn!(
                    "failed to remove file '{}' from '{}'",
                    attr.name(),
                    kobj.name()
                );
            }
        }
    }

    /// 在sysfs中，为指定的kobject创建一个动态申请的bin属性文件
    ///
    /// ## 参数
    ///
    /// - `kobj` 要创建属性文件的kobject
    /// - `attr` 属性
    ///
    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/fs/sysfs/file.c#558
    pub fn create_bin_file(
        &self,
        kobj: &Arc<dyn KObject>,
        attr: &Arc<dyn BinAttribute>,
    ) -> Result<(), SystemError> {
        let inode = kobj.inode().ok_or(SystemError::EINVAL)?;
        return self.add_bin_file_with_mode(&inode, attr, attr.mode());
    }

    /// 在sysfs中删除某个kobject的bin属性文件
    ///
    /// 如果属性文件不存在，则发出一个警告
    #[allow(dead_code)]
    pub fn remove_bin_file(&self, kobj: &Arc<dyn KObject>, attr: &Arc<dyn BinAttribute>) {
        let parent = kobj.inode();

        if let Some(parent) = parent {
            let r = parent.remove(attr.name());
            if unlikely(r.is_err()) {
                warn!(
                    "failed to remove file '{}' from '{}'",
                    attr.name(),
                    kobj.name()
                );
            }
        }
    }

    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/fs/sysfs/file.c#304
    pub(super) fn add_bin_file_with_mode(
        &self,
        parent: &Arc<KernFSInode>,
        attr: &Arc<dyn BinAttribute>,
        mode: ModeType,
    ) -> Result<(), SystemError> {
        let x = parent.private_data_mut();
        let kobj: Arc<dyn KObject>;
        if let Some(KernInodePrivateData::SysFS(SysFSKernPrivateData::Dir(dt))) = x.as_ref() {
            kobj = dt.kobj().unwrap();
        } else {
            drop(x);
            let path = self.kernfs_path(parent);
            panic!("parent '{path}' is not a dir");
        }
        drop(x);

        let kern_callback: &'static dyn KernFSCallback;
        let bin_support = attr.support_battr();

        if bin_support.contains(SysFSOpsSupport::BATTR_READ)
            && bin_support.contains(SysFSOpsSupport::BATTR_WRITE)
        {
            kern_callback = &PreallocKFOpsRW;
        } else if bin_support.contains(SysFSOpsSupport::BATTR_READ) {
            kern_callback = &PreallocKFOpsReadOnly;
        } else if bin_support.contains(SysFSOpsSupport::BATTR_WRITE) {
            kern_callback = &PreallocKFOpsWriteOnly;
        } else {
            kern_callback = &PreallocKFOpsEmpty;
        }

        let sys_priv =
            SysFSKernPrivateData::File(SysKernFilePriv::new(&kobj, None, Some(attr.clone())));
        let r = parent.add_file(
            attr.name().to_string(),
            mode.bitand(ModeType::from_bits_truncate(0o777)),
            Some(attr.size()),
            Some(KernInodePrivateData::SysFS(sys_priv)),
            Some(kern_callback),
        );

        if let Err(e) = r {
            if e == SystemError::EEXIST {
                self.warn_duplicate(parent, attr.name());
            }

            return Err(e);
        }
        return Ok(());
    }
}

#[derive(Debug)]
struct PreallocKFOpsRW;

impl KernFSCallback for PreallocKFOpsRW {
    fn open(&self, _data: KernCallbackData) -> Result<(), SystemError> {
        return Ok(());
    }

    fn read(
        &self,
        data: KernCallbackData,
        buf: &mut [u8],
        offset: usize,
    ) -> Result<usize, SystemError> {
        return data.callback_read(buf, offset);
    }

    fn write(
        &self,
        data: KernCallbackData,
        buf: &[u8],
        offset: usize,
    ) -> Result<usize, SystemError> {
        return data.callback_write(buf, offset);
    }

    #[inline]
    fn poll(&self, _data: KernCallbackData) -> Result<PollStatus, SystemError> {
        return Ok(PollStatus::READ | PollStatus::WRITE);
    }
}

#[derive(Debug)]
struct PreallocKFOpsReadOnly;

impl KernFSCallback for PreallocKFOpsReadOnly {
    fn open(&self, _data: KernCallbackData) -> Result<(), SystemError> {
        return Ok(());
    }

    fn read(
        &self,
        data: KernCallbackData,
        buf: &mut [u8],
        offset: usize,
    ) -> Result<usize, SystemError> {
        return data.callback_read(buf, offset);
    }

    fn write(
        &self,
        _data: KernCallbackData,
        _buf: &[u8],
        _offset: usize,
    ) -> Result<usize, SystemError> {
        return Err(SystemError::EPERM);
    }

    #[inline]
    fn poll(&self, _data: KernCallbackData) -> Result<PollStatus, SystemError> {
        return Ok(PollStatus::READ);
    }
}

#[derive(Debug)]
struct PreallocKFOpsWriteOnly;

impl KernFSCallback for PreallocKFOpsWriteOnly {
    fn open(&self, _data: KernCallbackData) -> Result<(), SystemError> {
        return Ok(());
    }

    fn read(
        &self,
        _data: KernCallbackData,
        _buf: &mut [u8],
        _offset: usize,
    ) -> Result<usize, SystemError> {
        return Err(SystemError::EPERM);
    }

    fn write(
        &self,
        data: KernCallbackData,
        buf: &[u8],
        offset: usize,
    ) -> Result<usize, SystemError> {
        return data.callback_write(buf, offset);
    }

    #[inline]
    fn poll(&self, _data: KernCallbackData) -> Result<PollStatus, SystemError> {
        return Ok(PollStatus::WRITE);
    }
}

#[derive(Debug)]
struct PreallocKFOpsEmpty;

impl KernFSCallback for PreallocKFOpsEmpty {
    fn open(&self, _data: KernCallbackData) -> Result<(), SystemError> {
        return Ok(());
    }

    fn read(
        &self,
        _data: KernCallbackData,
        _buf: &mut [u8],
        _offset: usize,
    ) -> Result<usize, SystemError> {
        return Err(SystemError::EPERM);
    }

    fn write(
        &self,
        _data: KernCallbackData,
        _buf: &[u8],
        _offset: usize,
    ) -> Result<usize, SystemError> {
        return Err(SystemError::EPERM);
    }

    #[inline]
    fn poll(&self, _data: KernCallbackData) -> Result<PollStatus, SystemError> {
        return Ok(PollStatus::empty());
    }
}

pub fn sysfs_emit_str(buf: &mut [u8], s: &str) -> Result<usize, SystemError> {
    let len = if buf.len() > s.len() {
        s.len()
    } else {
        buf.len() - 1
    };
    buf[..len].copy_from_slice(&s.as_bytes()[..len]);
    buf[len] = b'\0';
    return Ok(len);
}
