use alloc::string::ToString;

use crate::debug::sysfs::debugfs_kobj;
use crate::driver::base::kobject::KObject;
use crate::filesystem::kernfs::callback::{KernCallbackData, KernFSCallback, KernFilePrivateData};
use crate::filesystem::vfs::{InodeMode, PollStatus};
use system_error::SystemError;

#[derive(Debug)]
struct RcuDirCallBack;

impl KernFSCallback for RcuDirCallBack {
    fn open(&self, _data: KernCallbackData) -> Result<(), SystemError> {
        Ok(())
    }

    fn read(
        &self,
        _data: KernCallbackData,
        _buf: &mut [u8],
        _offset: usize,
    ) -> Result<usize, SystemError> {
        Err(SystemError::EISDIR)
    }

    fn write(
        &self,
        _data: KernCallbackData,
        _buf: &[u8],
        _offset: usize,
    ) -> Result<usize, SystemError> {
        Err(SystemError::EISDIR)
    }

    fn poll(&self, _data: KernCallbackData) -> Result<PollStatus, SystemError> {
        Err(SystemError::EISDIR)
    }
}

#[derive(Debug)]
struct RcuSelftestCallBack;

impl KernFSCallback for RcuSelftestCallBack {
    fn open(&self, mut data: KernCallbackData) -> Result<(), SystemError> {
        let report = crate::rcu::run_debug_selftests();
        data.file_private_data_mut()
            .replace(KernFilePrivateData::RcuSelftestReport(report));
        Ok(())
    }

    fn read(
        &self,
        data: KernCallbackData,
        buf: &mut [u8],
        offset: usize,
    ) -> Result<usize, SystemError> {
        let report = match data.file_private_data() {
            Some(KernFilePrivateData::RcuSelftestReport(report)) => report,
            _ => return Err(SystemError::EINVAL),
        };
        let bytes = report.as_bytes();
        if offset >= bytes.len() {
            return Ok(0);
        }

        let len = buf.len().min(bytes.len() - offset);
        buf[..len].copy_from_slice(&bytes[offset..offset + len]);
        Ok(len)
    }

    fn write(
        &self,
        _data: KernCallbackData,
        _buf: &[u8],
        _offset: usize,
    ) -> Result<usize, SystemError> {
        Err(SystemError::EPERM)
    }

    fn poll(&self, _data: KernCallbackData) -> Result<PollStatus, SystemError> {
        Ok(PollStatus::READ)
    }
}

pub fn init_debugfs_rcu() -> Result<(), SystemError> {
    let debugfs = debugfs_kobj();
    let root_dir = debugfs.inode().ok_or(SystemError::ENOENT)?;
    let rcu_root = root_dir.add_dir(
        "rcu".to_string(),
        InodeMode::from_bits_truncate(0o555),
        None,
        Some(&RcuDirCallBack),
    )?;

    rcu_root.add_file(
        "selftest".to_string(),
        InodeMode::S_IRUGO,
        Some(4096),
        None,
        Some(&RcuSelftestCallBack),
    )?;

    Ok(())
}
