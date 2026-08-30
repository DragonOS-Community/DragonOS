use alloc::string::{String, ToString};

use crate::debug::sysfs::debugfs_kobj;
use crate::driver::base::kobject::KObject;
use crate::filesystem::kernfs::callback::{KernCallbackData, KernFSCallback, KernFilePrivateData};
use crate::filesystem::vfs::{InodeMode, PollStatus};
use crate::libs::mutex::Mutex;
use system_error::SystemError;

lazy_static! {
    /// The production-path selftest intentionally holds a remote CPU until
    /// escalation. Run it once and serve immutable snapshots thereafter.
    static ref RCU_SELFTEST_REPORT: Mutex<Option<String>> = Mutex::new(None);
}

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

#[derive(Debug)]
struct RcuCallbacksCallBack;

#[derive(Debug)]
struct RcuStateCallBack;

#[derive(Debug)]
struct RcuStatsCallBack;

fn read_debug_text(
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

impl KernFSCallback for RcuSelftestCallBack {
    fn open(&self, mut data: KernCallbackData) -> Result<(), SystemError> {
        let report = {
            let mut cached = RCU_SELFTEST_REPORT.lock();
            cached
                .get_or_insert_with(crate::rcu::run_debug_selftests)
                .clone()
        };
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

impl KernFSCallback for RcuCallbacksCallBack {
    fn open(&self, mut data: KernCallbackData) -> Result<(), SystemError> {
        data.file_private_data_mut()
            .replace(KernFilePrivateData::DebugTextSnapshot(
                crate::rcu::callback_queue_debug_report(),
            ));
        Ok(())
    }

    fn read(
        &self,
        data: KernCallbackData,
        buf: &mut [u8],
        offset: usize,
    ) -> Result<usize, SystemError> {
        read_debug_text(data, buf, offset)
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

macro_rules! impl_rcu_snapshot_callback {
    ($callback:ty, $snapshot:path) => {
        impl KernFSCallback for $callback {
            fn open(&self, mut data: KernCallbackData) -> Result<(), SystemError> {
                data.file_private_data_mut()
                    .replace(KernFilePrivateData::DebugTextSnapshot($snapshot()));
                Ok(())
            }

            fn read(
                &self,
                data: KernCallbackData,
                buf: &mut [u8],
                offset: usize,
            ) -> Result<usize, SystemError> {
                read_debug_text(data, buf, offset)
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
    };
}

impl_rcu_snapshot_callback!(RcuStateCallBack, crate::rcu::state_debug_report);
impl_rcu_snapshot_callback!(RcuStatsCallBack, crate::rcu::stats_debug_report);

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
        InodeMode::S_IRUSR,
        Some(4096),
        None,
        Some(&RcuSelftestCallBack),
    )?;
    rcu_root.add_file(
        "callbacks".to_string(),
        InodeMode::S_IRUGO,
        Some(32 * 1024),
        None,
        Some(&RcuCallbacksCallBack),
    )?;
    rcu_root.add_file(
        "state".to_string(),
        InodeMode::S_IRUGO,
        Some(32 * 1024),
        None,
        Some(&RcuStateCallBack),
    )?;
    rcu_root.add_file(
        "stats".to_string(),
        InodeMode::S_IRUGO,
        Some(4096),
        None,
        Some(&RcuStatsCallBack),
    )?;

    Ok(())
}
