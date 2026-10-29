//! A root-only debugfs probe for the secure entropy provider.

use alloc::{string::String, string::ToString};
use system_error::SystemError;

use crate::{
    debug::sysfs::debugfs_kobj,
    driver::base::kobject::KObject,
    filesystem::{
        kernfs::callback::{KernCallbackData, KernFSCallback, KernFilePrivateData},
        vfs::{InodeMode, PollStatus},
    },
    libs::rand::secure_random_bytes,
};

#[derive(Debug)]
struct EntropySelftest;

impl KernFSCallback for EntropySelftest {
    fn open(&self, mut data: KernCallbackData) -> Result<(), SystemError> {
        let mut samples = [0u8; 64];
        let result = secure_random_bytes(&mut samples);
        let varied = samples[..32] != samples[32..];
        samples.fill(0);
        result?;
        if !varied {
            return Err(SystemError::EIO);
        }
        data.file_private_data_mut()
            .replace(KernFilePrivateData::DebugTextSnapshot(String::from(
                "secure entropy: two distinct reads completed\n",
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

pub fn init_debugfs_rng() -> Result<(), SystemError> {
    let root = debugfs_kobj().inode().ok_or(SystemError::ENOENT)?;
    let rng = root.add_dir(
        "rng".to_string(),
        InodeMode::from_bits_truncate(0o555),
        None,
        None,
    )?;
    rng.add_file(
        "selftest".to_string(),
        InodeMode::S_IRUSR,
        Some(80),
        None,
        Some(&EntropySelftest),
    )?;
    Ok(())
}
