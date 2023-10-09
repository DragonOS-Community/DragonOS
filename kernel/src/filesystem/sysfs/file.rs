use core::{intrinsics::unlikely, ops::BitAnd};

use alloc::{string::ToString, sync::Arc};

use crate::{
    driver::base::kobject::KObject,
    filesystem::{
        kernfs::{
            callback::{KernCallbackData, KernFSCallback, KernInodePrivateData},
            KernFSInode,
        },
        sysfs::SysFSOpsSupport,
        vfs::{syscall::ModeType, PollStatus},
    },
    kwarn,
    syscall::SystemError,
};

use super::{Attribute, SysFS, SysFSKernPrivateData};

#[derive(Debug)]
pub struct SysKernFilePriv {
    attribute: Option<&'static dyn Attribute>,
    // todo: 增加bin attribute,它和attribute二选一，只能有一个为Some
}

impl SysKernFilePriv {
    pub fn new(attribute: Option<&'static dyn Attribute>) -> Self {
        if attribute.is_none() {
            panic!("attribute can't be None");
        }
        return Self { attribute };
    }

    pub fn attribute(&self) -> Option<&'static dyn Attribute> {
        self.attribute
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

    // https://opengrok.ringotek.cn/xref/linux-6.1.9/fs/sysfs/file.c?fi=sysfs_add_file_mode_ns#271
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

        let sysfs_ops = kobj.kobj_type().unwrap().sysfs_ops().ok_or_else(|| {
            kwarn!("missing sysfs attribute operations for kobject: {kobj:?}");
            SystemError::EINVAL
        })?;

        // assume that all sysfs ops are preallocated.

        let sys_support = sysfs_ops.support(attr);
        let kern_callback: &'static dyn KernFSCallback;
        if sys_support.contains(SysFSOpsSupport::SHOW)
            && sys_support.contains(SysFSOpsSupport::STORE)
        {
            kern_callback = &PreallocKFOpsRW;
        } else if sys_support.contains(SysFSOpsSupport::SHOW) {
            kern_callback = &PreallocKFOpsReadOnly;
        } else if sys_support.contains(SysFSOpsSupport::STORE) {
            kern_callback = &PreallocKFOpsWriteOnly;
        } else {
            kern_callback = &PreallocKFOpsEmpty;
        }

        let sys_priv = SysFSKernPrivateData::File(SysKernFilePriv::new(Some(attr)));
        let r = parent.add_file(
            attr.name().to_string(),
            mode.bitand(ModeType::from_bits_truncate(0o777)),
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
                kwarn!(
                    "failed to remove file '{}' from '{}'",
                    attr.name(),
                    kobj.name()
                );
            }
        }
    }
}

#[derive(Debug)]
struct PreallocKFOpsRW;

impl KernFSCallback for PreallocKFOpsRW {
    fn open(&self, data: KernCallbackData) -> Result<(), SystemError> {
        return Ok(());
    }

    fn read(
        &self,
        data: KernCallbackData,
        buf: &mut [u8],
        offset: usize,
    ) -> Result<usize, SystemError> {
        todo!("PreallocKFOpsRW::read")
    }

    fn write(
        &self,
        data: KernCallbackData,
        buf: &[u8],
        offset: usize,
    ) -> Result<usize, SystemError> {
        todo!("PreallocKFOpsRW::write")
    }

    #[inline]
    fn poll(&self, data: KernCallbackData) -> Result<PollStatus, SystemError> {
        return Ok(PollStatus::READ | PollStatus::WRITE);
    }
}

#[derive(Debug)]
struct PreallocKFOpsReadOnly;

impl KernFSCallback for PreallocKFOpsReadOnly {
    fn open(&self, data: KernCallbackData) -> Result<(), SystemError> {
        return Ok(());
    }

    fn read(
        &self,
        data: KernCallbackData,
        buf: &mut [u8],
        offset: usize,
    ) -> Result<usize, SystemError> {
        todo!("PreallocKFOpsReadOnly::read")
    }

    fn write(
        &self,
        data: KernCallbackData,
        buf: &[u8],
        offset: usize,
    ) -> Result<usize, SystemError> {
        return Err(SystemError::EPERM);
    }

    #[inline]
    fn poll(&self, data: KernCallbackData) -> Result<PollStatus, SystemError> {
        return Ok(PollStatus::READ);
    }
}

#[derive(Debug)]
struct PreallocKFOpsWriteOnly;

impl KernFSCallback for PreallocKFOpsWriteOnly {
    fn open(&self, data: KernCallbackData) -> Result<(), SystemError> {
        return Ok(());
    }

    fn read(
        &self,
        data: KernCallbackData,
        buf: &mut [u8],
        offset: usize,
    ) -> Result<usize, SystemError> {
        return Err(SystemError::EPERM);
    }

    fn write(
        &self,
        data: KernCallbackData,
        buf: &[u8],
        offset: usize,
    ) -> Result<usize, SystemError> {
        todo!("PreallocKFOpsWriteOnly::write")
    }

    #[inline]
    fn poll(&self, data: KernCallbackData) -> Result<PollStatus, SystemError> {
        return Ok(PollStatus::WRITE);
    }
}

#[derive(Debug)]
struct PreallocKFOpsEmpty;

impl KernFSCallback for PreallocKFOpsEmpty {
    fn open(&self, data: KernCallbackData) -> Result<(), SystemError> {
        return Ok(());
    }

    fn read(
        &self,
        data: KernCallbackData,
        buf: &mut [u8],
        offset: usize,
    ) -> Result<usize, SystemError> {
        return Err(SystemError::EPERM);
    }

    fn write(
        &self,
        data: KernCallbackData,
        buf: &[u8],
        offset: usize,
    ) -> Result<usize, SystemError> {
        return Err(SystemError::EPERM);
    }

    #[inline]
    fn poll(&self, data: KernCallbackData) -> Result<PollStatus, SystemError> {
        return Ok(PollStatus::empty());
    }
}
