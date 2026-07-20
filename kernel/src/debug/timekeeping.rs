use alloc::{format, string::String, string::ToString};

use system_error::SystemError;

use crate::{
    debug::sysfs::debugfs_kobj,
    driver::base::kobject::KObject,
    filesystem::{
        kernfs::callback::{KernCallbackData, KernFSCallback, KernFilePrivateData},
        vfs::{InodeMode, PollStatus},
    },
    libs::rwlock::run_rwlock_selftests,
    time::{clocksource::run_clocksource_selftests, timekeeping::run_timekeeping_selftests},
};

#[cfg(target_arch = "x86_64")]
use crate::driver::clocksource::kvm_clock::run_kvm_clock_allocator_selftests;

#[derive(Debug)]
struct TimekeepingDirCallback;

impl KernFSCallback for TimekeepingDirCallback {
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
struct TimekeepingSelftestCallback;

impl KernFSCallback for TimekeepingSelftestCallback {
    fn open(&self, mut data: KernCallbackData) -> Result<(), SystemError> {
        data.file_private_data_mut()
            .replace(KernFilePrivateData::DebugTextSnapshot(run_selftests()));
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

fn run_selftests() -> String {
    let (timekeeping_passed, timekeeping_failed, timekeeping_body) = run_timekeeping_selftests();
    let (clocksource_passed, clocksource_failed, clocksource_body) = run_clocksource_selftests();
    let (rwlock_passed, rwlock_failed, rwlock_body) = run_rwlock_selftests();
    #[cfg(target_arch = "x86_64")]
    let (kvm_passed, kvm_failed, kvm_body) = run_kvm_clock_allocator_selftests();
    #[cfg(not(target_arch = "x86_64"))]
    let (kvm_passed, kvm_failed, kvm_body) = (0, 0, String::new());
    let passed = timekeeping_passed + clocksource_passed + rwlock_passed + kvm_passed;
    let failed = timekeeping_failed + clocksource_failed + rwlock_failed + kvm_failed;
    let status = if failed == 0 { "ok" } else { "fail" };
    format!(
        "status={status}\n{timekeeping_body}{clocksource_body}{rwlock_body}{kvm_body}summary_pass={passed}\nsummary_fail={failed}\n"
    )
}

pub fn init_debugfs_timekeeping() -> Result<(), SystemError> {
    let debugfs = debugfs_kobj();
    let root_dir = debugfs.inode().ok_or(SystemError::ENOENT)?;
    let timekeeping_root = root_dir.add_dir(
        "timekeeping".to_string(),
        InodeMode::from_bits_truncate(0o500),
        None,
        Some(&TimekeepingDirCallback),
    )?;
    timekeeping_root.add_file(
        "selftest".to_string(),
        InodeMode::S_IRUSR,
        Some(16 * 1024),
        None,
        Some(&TimekeepingSelftestCallback),
    )?;
    Ok(())
}
