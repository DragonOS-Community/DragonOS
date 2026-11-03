//! Scheduler user-return probes, present only in sched_selftest builds.

use alloc::string::ToString;
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
struct RseqNotifySelftest;

impl KernFSCallback for RseqNotifySelftest {
    fn open(&self, mut data: KernCallbackData) -> Result<(), SystemError> {
        let report = crate::process::rseq::selftest::run()?;
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
        let count = buf.len().min(bytes.len() - offset);
        buf[..count].copy_from_slice(&bytes[offset..offset + count]);
        Ok(count)
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

pub fn init_debugfs_sched() -> Result<(), SystemError> {
    let root = debugfs_kobj().inode().ok_or(SystemError::ENOENT)?;
    let sched = root.add_dir(
        "sched".to_string(),
        InodeMode::from_bits_truncate(0o500),
        None,
        None,
    )?;
    sched.add_file(
        "rseq_notify_selftest".to_string(),
        InodeMode::S_IRUSR,
        Some(256),
        None,
        Some(&RseqNotifySelftest),
    )?;
    Ok(())
}
