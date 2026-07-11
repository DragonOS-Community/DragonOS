use alloc::{string::String, string::ToString};

use crate::{
    debug::sysfs::debugfs_kobj,
    driver::base::kobject::KObject,
    filesystem::{
        kernfs::callback::{KernCallbackData, KernFSCallback, KernFilePrivateData},
        vfs::{InodeMode, PollStatus},
    },
};
use system_error::SystemError;

#[derive(Debug)]
struct Ext4LifecycleSelftestCallback;

impl KernFSCallback for Ext4LifecycleSelftestCallback {
    fn open(&self, mut data: KernCallbackData) -> Result<(), SystemError> {
        let report = crate::filesystem::ext4::inode::run_lifecycle_selftests();
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

pub fn init_debugfs_ext4() -> Result<(), SystemError> {
    let debugfs = debugfs_kobj();
    let root = debugfs.inode().ok_or(SystemError::ENOENT)?;
    let ext4 = root.add_dir(
        "ext4".to_string(),
        InodeMode::from_bits_truncate(0o555),
        None,
        None,
    )?;
    ext4.add_file(
        "lifecycle_selftest".to_string(),
        InodeMode::S_IRUGO,
        Some(4096),
        None,
        Some(&Ext4LifecycleSelftestCallback),
    )?;
    Ok(())
}
