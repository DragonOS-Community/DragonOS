use alloc::{string::String, string::ToString};

use system_error::SystemError;

use crate::{
    debug::sysfs::debugfs_kobj,
    driver::base::kobject::KObject,
    filesystem::{
        kernfs::callback::{KernCallbackData, KernFSCallback, KernFilePrivateData},
        vfs::{InodeMode, PollStatus},
    },
};

#[derive(Debug)]
struct MountWrapperCacheSelftestCallback;

impl KernFSCallback for MountWrapperCacheSelftestCallback {
    fn open(&self, mut data: KernCallbackData) -> Result<(), SystemError> {
        let report = crate::filesystem::vfs::mount::run_mount_wrapper_cache_debug_selftest()?;
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
        let report: &String = match data.file_private_data() {
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

pub fn init_debugfs_vfs() -> Result<(), SystemError> {
    let debugfs = debugfs_kobj();
    let root = debugfs.inode().ok_or(SystemError::ENOENT)?;
    let vfs = root.add_dir(
        "vfs".to_string(),
        InodeMode::from_bits_truncate(0o555),
        None,
        None,
    )?;
    vfs.add_file(
        "mount_wrapper_cache_selftest".to_string(),
        InodeMode::S_IRUSR,
        Some(4096),
        None,
        Some(&MountWrapperCacheSelftestCallback),
    )?;
    Ok(())
}
