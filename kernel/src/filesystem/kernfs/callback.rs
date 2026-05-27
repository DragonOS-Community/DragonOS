use super::KernFSInode;
use crate::filesystem::vfs::file::FilePrivateData;
use crate::filesystem::{sysfs::SysFSKernPrivateData, vfs::PollStatus};
use crate::libs::mutex::MutexGuard;
use crate::tracepoint::{TraceCmdLineCacheSnapshot, TracePipeSnapshot, TracePointInfo};
use alloc::{string::String, sync::Arc};
use core::fmt::Debug;
use system_error::SystemError;

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
    inode_private_data: MutexGuard<'a, Option<KernInodePrivateData>>,
    file_private_data: MutexGuard<'a, FilePrivateData>,
}

#[allow(dead_code)]
impl<'a> KernCallbackData<'a> {
    pub fn new(
        kern_inode: Arc<KernFSInode>,
        inode_private_data: MutexGuard<'a, Option<KernInodePrivateData>>,
        mut file_private_data: MutexGuard<'a, FilePrivateData>,
    ) -> Self {
        if !matches!(*file_private_data, FilePrivateData::Kernfs(_)) {
            *file_private_data = FilePrivateData::Kernfs(None);
        }
        Self {
            kern_inode,
            inode_private_data,
            file_private_data,
        }
    }

    #[inline(always)]
    pub fn kern_inode(&self) -> &Arc<KernFSInode> {
        return &self.kern_inode;
    }

    #[inline(always)]
    pub fn private_data(&self) -> &Option<KernInodePrivateData> {
        return &self.inode_private_data;
    }

    #[inline(always)]
    pub fn private_data_mut(&mut self) -> &mut Option<KernInodePrivateData> {
        return &mut self.inode_private_data;
    }

    #[inline(always)]
    pub fn file_private_data(&self) -> Option<&KernFilePrivateData> {
        match &*self.file_private_data {
            FilePrivateData::Kernfs(private_data) => private_data.as_ref(),
            _ => None,
        }
    }

    #[inline(always)]
    pub fn file_private_data_mut(&mut self) -> &mut Option<KernFilePrivateData> {
        match &mut *self.file_private_data {
            FilePrivateData::Kernfs(private_data) => private_data,
            _ => panic!("kernfs callback file private data is not initialized"),
        }
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
    DebugFS(Arc<TracePointInfo>),
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum KernFilePrivateData {
    TracePipe(TracePipeSnapshot),
    TraceSavedCmdlines(TraceCmdLineCacheSnapshot),
    RcuSelftestReport(String),
    ErrSeqSelftestReport(String),
}

impl KernInodePrivateData {
    #[inline(always)]
    pub fn callback_read(&self, buf: &mut [u8], offset: usize) -> Result<usize, SystemError> {
        return match self {
            KernInodePrivateData::SysFS(private_data) => private_data.callback_read(buf, offset),
            _ => Err(SystemError::ENOSYS),
        };
    }

    #[inline(always)]
    pub fn callback_write(&self, buf: &[u8], offset: usize) -> Result<usize, SystemError> {
        return match self {
            KernInodePrivateData::SysFS(private_data) => private_data.callback_write(buf, offset),
            _ => Err(SystemError::ENOSYS),
        };
    }
}
