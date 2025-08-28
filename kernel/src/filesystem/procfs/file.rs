use alloc::sync::Arc;
use system_error::SystemError;

use crate::filesystem::kernfs::callback::{KernCallbackData, KernFSCallback, KernInodePrivateData};
use crate::filesystem::vfs::PollStatus;

use super::ProcFSKernPrivateData;

/// 只读文件回调
#[derive(Debug)]
pub(super) struct ProcFSCallbackReadOnly;

impl KernFSCallback for ProcFSCallbackReadOnly {
    fn open(&self, _data: KernCallbackData) -> Result<(), SystemError> {
        Ok(())
    }

    fn read(&self, data: KernCallbackData, buf: &mut [u8], offset: usize) -> Result<usize, SystemError> {
        use ::log::info;
        // info!("ProcFSCallbackReadOnly::read called, buf_len={}, offset={}", buf.len(), offset);
        let result = data.callback_read(buf, offset);
        // info!("ProcFSCallbackReadOnly::read result: {:?}", result);
        result
    }

    fn write(&self, _data: KernCallbackData, _buf: &[u8], _offset: usize) -> Result<usize, SystemError> {
        Err(SystemError::EPERM)
    }

    fn poll(&self, _data: KernCallbackData) -> Result<PollStatus, SystemError> {
        Ok(PollStatus::READ)
    }
}

/// 只写文件回调
#[derive(Debug)]
pub(super) struct ProcFSCallbackWriteOnly;

impl KernFSCallback for ProcFSCallbackWriteOnly {
    fn open(&self, _data: KernCallbackData) -> Result<(), SystemError> {
        Ok(())
    }

    fn read(&self, _data: KernCallbackData, _buf: &mut [u8], _offset: usize) -> Result<usize, SystemError> {
        Err(SystemError::EPERM)
    }

    fn write(&self, data: KernCallbackData, buf: &[u8], offset: usize) -> Result<usize, SystemError> {
        data.callback_write(buf, offset)
    }

    fn poll(&self, _data: KernCallbackData) -> Result<PollStatus, SystemError> {
        Ok(PollStatus::WRITE)
    }
}

/// 读写文件回调
#[derive(Debug)]
pub(super) struct ProcFSCallbackRW;

impl KernFSCallback for ProcFSCallbackRW {
    fn open(&self, _data: KernCallbackData) -> Result<(), SystemError> {
        Ok(())
    }

    fn read(&self, data: KernCallbackData, buf: &mut [u8], offset: usize) -> Result<usize, SystemError> {
        data.callback_read(buf, offset)
    }

    fn write(&self, data: KernCallbackData, buf: &[u8], offset: usize) -> Result<usize, SystemError> {
        data.callback_write(buf, offset)
    }

    fn poll(&self, _data: KernCallbackData) -> Result<PollStatus, SystemError> {
        Ok(PollStatus::READ | PollStatus::WRITE)
    }
}

/// 空操作回调
#[derive(Debug)]
pub(super) struct ProcFSCallbackEmpty;

impl KernFSCallback for ProcFSCallbackEmpty {
    fn open(&self, _data: KernCallbackData) -> Result<(), SystemError> {
        Ok(())
    }

    fn read(&self, _data: KernCallbackData, _buf: &mut [u8], _offset: usize) -> Result<usize, SystemError> {
        Err(SystemError::EPERM)
    }

    fn write(&self, _data: KernCallbackData, _buf: &[u8], _offset: usize) -> Result<usize, SystemError> {
        Err(SystemError::EPERM)
    }

    fn poll(&self, _data: KernCallbackData) -> Result<PollStatus, SystemError> {
        Ok(PollStatus::empty())
    }
}

// 全局回调实例
pub(super) static PROCFS_CALLBACK_RO: ProcFSCallbackReadOnly = ProcFSCallbackReadOnly;
pub(super) static PROCFS_CALLBACK_WO: ProcFSCallbackWriteOnly = ProcFSCallbackWriteOnly;
pub(super) static PROCFS_CALLBACK_RW: ProcFSCallbackRW = ProcFSCallbackRW;
pub(super) static PROCFS_CALLBACK_EMPTY: ProcFSCallbackEmpty = ProcFSCallbackEmpty;