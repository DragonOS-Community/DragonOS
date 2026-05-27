use alloc::{format, string::String, string::ToString};
use core::str;

use crate::debug::sysfs::debugfs_kobj;
use crate::driver::base::kobject::KObject;
use crate::filesystem::kernfs::callback::{KernCallbackData, KernFSCallback, KernFilePrivateData};
use crate::filesystem::vfs::mount::MountFSInode;
use crate::filesystem::vfs::{InodeMode, PollStatus};
use crate::libs::casting::DowncastArc;
use crate::libs::errseq::ErrSeq;
use crate::process::ProcessManager;
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

#[derive(Debug)]
struct ErrSeqInjectCallBack;

#[derive(Debug, Clone, Copy)]
enum InjectTarget {
    Mapping,
    Superblock,
}

impl KernFSCallback for ErrSeqInjectCallBack {
    fn open(&self, _data: KernCallbackData) -> Result<(), SystemError> {
        Ok(())
    }

    fn read(
        &self,
        _data: KernCallbackData,
        buf: &mut [u8],
        offset: usize,
    ) -> Result<usize, SystemError> {
        let help = b"usage: mapping|superblock <fd> EIO|ENOSPC|errno\n";
        if offset >= help.len() {
            return Ok(0);
        }

        let len = buf.len().min(help.len() - offset);
        buf[..len].copy_from_slice(&help[offset..offset + len]);
        Ok(len)
    }

    fn write(
        &self,
        _data: KernCallbackData,
        buf: &[u8],
        _offset: usize,
    ) -> Result<usize, SystemError> {
        let command = parse_inject_command(buf)?;
        inject_writeback_error(command)?;
        Ok(buf.len())
    }

    fn poll(&self, _data: KernCallbackData) -> Result<PollStatus, SystemError> {
        Ok(PollStatus::WRITE)
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

    let errseq = ErrSeq::new();
    let mut first = errseq.sample();
    let mut second = errseq.sample();
    errseq.set(SystemError::EIO);
    let pagecache_multi_fd_ok = errseq.check_and_advance(&mut first) == Some(SystemError::EIO)
        && errseq.check_and_advance(&mut second) == Some(SystemError::EIO)
        && errseq.check_and_advance(&mut first).is_none();
    append_case(
        &mut report,
        "pagecache_multi_fd",
        pagecache_multi_fd_ok,
        &mut failures,
    );

    let errseq = ErrSeq::new();
    let mut first = errseq.sample();
    let mut second = errseq.sample();
    errseq.set(SystemError::ENOSPC);
    let syncfs_sb_cursor_ok = errseq.check_and_advance(&mut first) == Some(SystemError::ENOSPC)
        && errseq.check_and_advance(&mut second) == Some(SystemError::ENOSPC)
        && errseq.check_and_advance(&mut second).is_none();
    append_case(
        &mut report,
        "syncfs_sb_cursor",
        syncfs_sb_cursor_ok,
        &mut failures,
    );

    let errseq = ErrSeq::new();
    let mut cursor = errseq.sample();
    errseq.set(SystemError::EIO);
    let sync_file_range_wait_ok = errseq.check(cursor) == Some(SystemError::EIO)
        && errseq.check_and_advance(&mut cursor) == Some(SystemError::EIO)
        && errseq.check_and_advance(&mut cursor).is_none();
    append_case(
        &mut report,
        "sync_file_range_wait",
        sync_file_range_wait_ok,
        &mut failures,
    );

    let errseq = ErrSeq::new();
    let mut cursor = errseq.sample();
    errseq.set(SystemError::EIO);
    let msync_range_ok = errseq.check_and_advance(&mut cursor) == Some(SystemError::EIO)
        && errseq.check_and_advance(&mut cursor).is_none();
    append_case(&mut report, "msync_range", msync_range_ok, &mut failures);

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

fn parse_inject_command(buf: &[u8]) -> Result<(InjectTarget, i32, SystemError), SystemError> {
    let command = str::from_utf8(buf).map_err(|_| SystemError::EINVAL)?;
    let mut parts = command.split_whitespace();
    let target = match parts.next() {
        Some("mapping") => InjectTarget::Mapping,
        Some("superblock") => InjectTarget::Superblock,
        _ => return Err(SystemError::EINVAL),
    };
    let fd = parts
        .next()
        .ok_or(SystemError::EINVAL)?
        .parse::<i32>()
        .map_err(|_| SystemError::EINVAL)?;
    let error = parse_error(parts.next().ok_or(SystemError::EINVAL)?)?;
    if parts.next().is_some() {
        return Err(SystemError::EINVAL);
    }
    Ok((target, fd, error))
}

fn parse_error(token: &str) -> Result<SystemError, SystemError> {
    match token {
        "EIO" => Ok(SystemError::EIO),
        "ENOSPC" => Ok(SystemError::ENOSPC),
        "EDQUOT" => Ok(SystemError::EDQUOT),
        _ => {
            let errno = token.parse::<i32>().map_err(|_| SystemError::EINVAL)?;
            let posix_errno = if errno < 0 { errno } else { -errno };
            SystemError::from_posix_errno(posix_errno).ok_or(SystemError::EINVAL)
        }
    }
}

fn inject_writeback_error(
    (target, fd, error): (InjectTarget, i32, SystemError),
) -> Result<(), SystemError> {
    let file = {
        let binding = ProcessManager::current_pcb().fd_table();
        let fd_table_guard = binding.read();
        fd_table_guard
            .get_file_by_fd(fd)
            .ok_or(SystemError::EBADF)?
    };

    match target {
        InjectTarget::Mapping => {
            let page_cache = file.inode().page_cache().ok_or(SystemError::EINVAL)?;
            page_cache.record_writeback_error_with_superblock(error);
            Ok(())
        }
        InjectTarget::Superblock => {
            let mount_inode = file
                .inode()
                .downcast_arc::<MountFSInode>()
                .ok_or(SystemError::EINVAL)?;
            mount_inode.mount_fs().record_wb_error(error);
            Ok(())
        }
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

    errseq_root.add_file(
        "inject".to_string(),
        InodeMode::S_IRUSR | InodeMode::S_IWUSR,
        Some(4096),
        None,
        Some(&ErrSeqInjectCallBack),
    )?;

    Ok(())
}
