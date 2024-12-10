use crate::{
    filesystem::{sysfs::SysFSKernPrivateData, vfs::PollStatus},
    libs::spinlock::SpinLockGuard,
};
use alloc::sync::Arc;
use core::fmt::Debug;
use system_error::SystemError;

use super::KernFSInode;

/// KernFS文件的回调接口
///
/// 当用户态程序打开、读取、写入、关闭文件时，kernfs会调用相应的回调函数。
pub trait KernFSCallback: Send + Sync + Debug {
    fn open(&self, data: KernCallbackData) -> Result<(), SystemError>;

    fn read(
        &self,
        data: KernCallbackData,
        buf: &mut [u8],
        offset: usize,
    ) -> Result<usize, SystemError>;

    fn write(
        &self,
        data: KernCallbackData,
        buf: &[u8],
        offset: usize,
    ) -> Result<usize, SystemError>;

    fn poll(&self, data: KernCallbackData) -> Result<PollStatus, SystemError>;
}

/// KernFS文件的回调数据
#[derive(Debug)]
pub struct KernCallbackData<'a> {
    kern_inode: Arc<KernFSInode>,
    private_data: SpinLockGuard<'a, Option<KernInodePrivateData>>,
}

#[allow(dead_code)]
impl<'a> KernCallbackData<'a> {
    pub fn new(
        kern_inode: Arc<KernFSInode>,
        private_data: SpinLockGuard<'a, Option<KernInodePrivateData>>,
    ) -> Self {
        Self {
            kern_inode,
            private_data,
        }
    }

    #[inline(always)]
    pub fn kern_inode(&self) -> &Arc<KernFSInode> {
        return &self.kern_inode;
    }

    #[inline(always)]
    pub fn private_data(&self) -> &Option<KernInodePrivateData> {
        return &self.private_data;
    }

    #[inline(always)]
    pub fn private_data_mut(&mut self) -> &mut Option<KernInodePrivateData> {
        return &mut self.private_data;
    }

    pub fn callback_read(&self, buf: &mut [u8], offset: usize) -> Result<usize, SystemError> {
        let private_data = self.private_data();
        if let Some(private_data) = private_data {
            return private_data.callback_read(buf, offset);
        }
        return Err(SystemError::ENOSYS);
    }

    pub fn callback_write(&self, buf: &[u8], offset: usize) -> Result<usize, SystemError> {
        let private_data = self.private_data();
        if let Some(private_data) = private_data {
            return private_data.callback_write(buf, offset);
        }
        return Err(SystemError::ENOSYS);
    }
}

#[allow(dead_code)]
#[derive(Debug)]
pub enum KernInodePrivateData {
    SysFS(SysFSKernPrivateData),
}

impl KernInodePrivateData {
    #[inline(always)]
    pub fn callback_read(&self, buf: &mut [u8], offset: usize) -> Result<usize, SystemError> {
        match self {
            KernInodePrivateData::SysFS(private_data) => {
                return private_data.callback_read(buf, offset);
            }
        }
    }

    #[inline(always)]
    pub fn callback_write(&self, buf: &[u8], offset: usize) -> Result<usize, SystemError> {
        match self {
            KernInodePrivateData::SysFS(private_data) => {
                return private_data.callback_write(buf, offset);
            }
        }
    }
}
