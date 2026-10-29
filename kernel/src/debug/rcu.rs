use alloc::string::{String, ToString};
use core::{
    str,
    sync::atomic::{AtomicBool, Ordering},
};

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
    static ref RCU_TORTURE_REPORT: Mutex<String> =
        Mutex::new("status=never-run\n".to_string());
}

static RCU_TORTURE_RUNNING: AtomicBool = AtomicBool::new(false);
static RCU_TORTURE_POISONED: AtomicBool = AtomicBool::new(false);

struct RcuTortureRunGuard;

impl Drop for RcuTortureRunGuard {
    fn drop(&mut self) {
        RCU_TORTURE_RUNNING.store(false, Ordering::Release);
    }
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

#[derive(Debug)]
struct SrcuStateCallBack;

#[derive(Debug)]
struct RcuTortureCallBack;

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

fn parse_u64(value: &str) -> Result<u64, SystemError> {
    if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        u64::from_str_radix(hex, 16).map_err(|_| SystemError::EINVAL)
    } else {
        value.parse::<u64>().map_err(|_| SystemError::EINVAL)
    }
}

fn parse_torture_config(buf: &[u8]) -> Result<crate::rcu::RcuTortureConfig, SystemError> {
    if buf.is_empty() || buf.len() > 128 {
        return Err(if buf.len() > 128 {
            SystemError::E2BIG
        } else {
            SystemError::EINVAL
        });
    }
    let input = str::from_utf8(buf).map_err(|_| SystemError::EINVAL)?.trim();
    let mut seed = None;
    let mut rounds = None;
    for token in input.split_whitespace() {
        let (key, value) = token.split_once('=').ok_or(SystemError::EINVAL)?;
        if value.is_empty() {
            return Err(SystemError::EINVAL);
        }
        match key {
            "seed" if seed.is_none() => seed = Some(parse_u64(value)?),
            "rounds" if rounds.is_none() => {
                rounds = Some(value.parse::<usize>().map_err(|_| SystemError::EINVAL)?)
            }
            _ => return Err(SystemError::EINVAL),
        }
    }
    let config = crate::rcu::RcuTortureConfig {
        seed: seed.ok_or(SystemError::EINVAL)?,
        rounds: rounds.ok_or(SystemError::EINVAL)?,
    };
    config.validate()
}

impl KernFSCallback for RcuTortureCallBack {
    fn open(&self, mut data: KernCallbackData) -> Result<(), SystemError> {
        data.file_private_data_mut()
            .replace(KernFilePrivateData::DebugTextSnapshot(
                RCU_TORTURE_REPORT.lock().clone(),
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
        buf: &[u8],
        offset: usize,
    ) -> Result<usize, SystemError> {
        if offset != 0 {
            return Err(SystemError::EINVAL);
        }
        let config = parse_torture_config(buf)?;
        if RCU_TORTURE_POISONED.load(Ordering::Acquire) {
            return Err(SystemError::EIO);
        }
        RCU_TORTURE_RUNNING
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| SystemError::EBUSY)?;
        let _guard = RcuTortureRunGuard;
        let result = crate::rcu::run_torture(config)?;
        if result.reboot_required {
            RCU_TORTURE_POISONED.store(true, Ordering::Release);
        }
        *RCU_TORTURE_REPORT.lock() = result.report;
        if result.passed {
            Ok(buf.len())
        } else {
            Err(SystemError::EIO)
        }
    }

    fn poll(&self, _data: KernCallbackData) -> Result<PollStatus, SystemError> {
        Ok(PollStatus::READ | PollStatus::WRITE)
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
impl_rcu_snapshot_callback!(SrcuStateCallBack, crate::rcu::srcu::state_debug_report);

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
    let srcu_root = rcu_root.add_dir(
        "srcu".to_string(),
        InodeMode::from_bits_truncate(0o555),
        None,
        Some(&RcuDirCallBack),
    )?;
    srcu_root.add_file(
        "state".to_string(),
        InodeMode::S_IRUGO,
        Some(32 * 1024),
        None,
        Some(&SrcuStateCallBack),
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
    rcu_root.add_file(
        "torture".to_string(),
        InodeMode::S_IRUSR | InodeMode::S_IWUSR,
        Some(4096),
        None,
        Some(&RcuTortureCallBack),
    )?;

    Ok(())
}
