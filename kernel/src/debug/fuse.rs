use alloc::string::ToString;
use core::str;

use crate::debug::sysfs::debugfs_kobj;
use crate::driver::base::kobject::KObject;
use crate::filesystem::kernfs::callback::{KernCallbackData, KernFSCallback, KernFilePrivateData};
use crate::filesystem::vfs::{InodeMode, PollStatus};
use system_error::SystemError;

#[derive(Debug)]
struct FuseStatsDirCallBack;

impl KernFSCallback for FuseStatsDirCallBack {
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
struct FuseStatsCallBack;

impl KernFSCallback for FuseStatsCallBack {
    fn open(&self, mut data: KernCallbackData) -> Result<(), SystemError> {
        let report = crate::filesystem::fuse::stats::format_snapshot();
        data.file_private_data_mut()
            .replace(KernFilePrivateData::DebugTextSnapshot(report));
        Ok(())
    }

    fn read(
        &self,
        data: KernCallbackData,
        buf: &mut [u8],
        offset: usize,
    ) -> Result<usize, SystemError> {
        let report = match data.file_private_data() {
            Some(KernFilePrivateData::DebugTextSnapshot(report)) => report,
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

#[derive(Debug)]
struct FuseStatsControlCallBack;

impl KernFSCallback for FuseStatsControlCallBack {
    fn open(&self, mut data: KernCallbackData) -> Result<(), SystemError> {
        let mode = crate::filesystem::fuse::stats::stats_mode();
        data.file_private_data_mut()
            .replace(KernFilePrivateData::DebugTextSnapshot(alloc::format!(
                "{}\n",
                mode.as_str()
            )));
        Ok(())
    }

    fn read(
        &self,
        data: KernCallbackData,
        buf: &mut [u8],
        offset: usize,
    ) -> Result<usize, SystemError> {
        let report = match data.file_private_data() {
            Some(KernFilePrivateData::DebugTextSnapshot(report)) => report,
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
        buf: &[u8],
        offset: usize,
    ) -> Result<usize, SystemError> {
        if offset != 0 || buf.is_empty() {
            return Err(SystemError::EINVAL);
        }
        let value = str::from_utf8(buf).map_err(|_| SystemError::EINVAL)?;
        let mode = crate::filesystem::fuse::stats::FuseStatsMode::parse(value)
            .map_err(|_| SystemError::EINVAL)?;
        crate::filesystem::fuse::stats::set_stats_mode(mode);
        Ok(buf.len())
    }

    fn poll(&self, _data: KernCallbackData) -> Result<PollStatus, SystemError> {
        Ok(PollStatus::READ | PollStatus::WRITE)
    }
}

pub fn init_debugfs_fuse() -> Result<(), SystemError> {
    let debugfs = debugfs_kobj();
    let root_dir = debugfs.inode().ok_or(SystemError::ENOENT)?;
    let fuse_root = root_dir.add_dir(
        "fuse".to_string(),
        InodeMode::from_bits_truncate(0o555),
        None,
        Some(&FuseStatsDirCallBack),
    )?;

    fuse_root.add_file(
        "stats".to_string(),
        InodeMode::S_IRUSR,
        Some(4096),
        None,
        Some(&FuseStatsCallBack),
    )?;

    fuse_root.add_file(
        "stats_mode".to_string(),
        InodeMode::S_IRUSR | InodeMode::S_IWUSR,
        Some(32),
        None,
        Some(&FuseStatsControlCallBack),
    )?;

    Ok(())
}
