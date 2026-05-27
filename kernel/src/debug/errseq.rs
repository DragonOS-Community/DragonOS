use alloc::{format, string::String, string::ToString};

use crate::debug::sysfs::debugfs_kobj;
use crate::driver::base::kobject::KObject;
use crate::filesystem::kernfs::callback::{KernCallbackData, KernFSCallback, KernFilePrivateData};
use crate::filesystem::vfs::{InodeMode, PollStatus};
use crate::libs::errseq::ErrSeq;
use system_error::SystemError;

#[derive(Debug)]
struct ErrSeqDirCallBack;

impl KernFSCallback for ErrSeqDirCallBack {
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
struct ErrSeqSelftestCallBack;

impl KernFSCallback for ErrSeqSelftestCallBack {
    fn open(&self, mut data: KernCallbackData) -> Result<(), SystemError> {
        let report = run_errseq_selftests();
        data.file_private_data_mut()
            .replace(KernFilePrivateData::ErrSeqSelftestReport(report));
        Ok(())
    }

    fn read(
        &self,
        data: KernCallbackData,
        buf: &mut [u8],
        offset: usize,
    ) -> Result<usize, SystemError> {
        let report = match data.file_private_data() {
            Some(KernFilePrivateData::ErrSeqSelftestReport(report)) => report,
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

fn run_errseq_selftests() -> String {
    let mut failures = 0usize;
    let mut report = String::new();

    let errseq = ErrSeq::new();
    errseq.set(SystemError::EIO);
    let mut sample = errseq.sample();
    let unseen_ok = sample == 0 && errseq.check_and_advance(&mut sample) == Some(SystemError::EIO);
    append_case(&mut report, "unseen_sample", unseen_ok, &mut failures);

    let errseq = ErrSeq::new();
    let mut first = errseq.sample();
    let mut second = errseq.sample();
    errseq.set(SystemError::ENOSPC);
    let multi_ok = errseq.check_and_advance(&mut first) == Some(SystemError::ENOSPC)
        && errseq.check_and_advance(&mut second) == Some(SystemError::ENOSPC)
        && errseq.check_and_advance(&mut first).is_none()
        && errseq.check_and_advance(&mut second).is_none();
    append_case(&mut report, "multi_watcher", multi_ok, &mut failures);

    let errseq = ErrSeq::new();
    let mut before = errseq.sample();
    errseq.set(SystemError::EIO);
    let _ = errseq.check_and_advance(&mut before);
    let mut late = errseq.sample();
    let late_ok = errseq.check_and_advance(&mut late).is_none();
    append_case(&mut report, "late_sample", late_ok, &mut failures);

    if failures == 0 {
        report.insert_str(0, "status=ok\n");
    } else {
        report.insert_str(0, &format!("status=fail failures={failures}\n"));
    }
    report
}

fn append_case(report: &mut String, name: &str, ok: bool, failures: &mut usize) {
    if ok {
        report.push_str(&format!("{name}=ok\n"));
    } else {
        *failures += 1;
        report.push_str(&format!("{name}=fail\n"));
    }
}

pub fn init_debugfs_errseq() -> Result<(), SystemError> {
    let debugfs = debugfs_kobj();
    let root_dir = debugfs.inode().ok_or(SystemError::ENOENT)?;
    let errseq_root = root_dir.add_dir(
        "errseq".to_string(),
        InodeMode::from_bits_truncate(0o555),
        None,
        Some(&ErrSeqDirCallBack),
    )?;

    errseq_root.add_file(
        "selftest".to_string(),
        InodeMode::S_IRUGO,
        Some(4096),
        None,
        Some(&ErrSeqSelftestCallBack),
    )?;

    Ok(())
}
