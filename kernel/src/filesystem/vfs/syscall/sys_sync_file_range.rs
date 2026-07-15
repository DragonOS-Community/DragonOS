//! sync_file_range 系统调用实现。
//!
//! 语义参考 Linux 6.6.21 `fs/sync.c`：该调用只控制文件数据页写回，
//! 不同步文件元数据，也不改变文件当前位置。

use alloc::string::ToString;
use alloc::vec::Vec;

use system_error::SystemError;

use crate::arch::{interrupt::TrapFrame, syscall::nr::SYS_SYNC_FILE_RANGE, MMArch};
use crate::filesystem::vfs::{
    file::{File, FileMode},
    FileType,
};
use crate::mm::MemoryManagementArch;
use crate::process::ProcessManager;
use crate::syscall::table::{FormattedSyscallParam, Syscall};

bitflags::bitflags! {
    struct SyncFileRangeFlags: u32 {
        const WAIT_BEFORE = 1;
        const WRITE = 2;
        const WAIT_AFTER = 4;
    }
}

pub struct SysSyncFileRangeHandle;

impl Syscall for SysSyncFileRangeHandle {
    fn num_args(&self) -> usize {
        4
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = args[0] as i32;
        let offset = args[1] as i64;
        let nbytes = args[2] as i64;
        let raw_flags = args[3] as u32;

        do_sync_file_range(fd, offset, nbytes, raw_flags)?;
        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fd", (args[0] as i32).to_string()),
            FormattedSyscallParam::new("offset", (args[1] as i64).to_string()),
            FormattedSyscallParam::new("nbytes", (args[2] as i64).to_string()),
            FormattedSyscallParam::new("flags", format!("{:#x}", args[3] as u32)),
        ]
    }
}

fn do_sync_file_range(
    fd: i32,
    offset: i64,
    nbytes: i64,
    raw_flags: u32,
) -> Result<(), SystemError> {
    let file = {
        let binding = ProcessManager::current_pcb().fd_table();
        let fd_table_guard = binding.read();
        fd_table_guard
            .get_file_by_fd(fd)
            .ok_or(SystemError::EBADF)?
    };

    if file.mode().contains(FileMode::FMODE_PATH) {
        return Err(SystemError::EBADF);
    }

    let flags = SyncFileRangeFlags::from_bits(raw_flags).ok_or(SystemError::EINVAL)?;

    let endbyte = offset.checked_add(nbytes).ok_or(SystemError::EINVAL)?;
    if offset < 0 || endbyte < 0 || endbyte < offset {
        return Err(SystemError::EINVAL);
    }

    sync_file_range(&file, offset as usize, nbytes as usize, flags)
}

fn sync_file_range(
    file: &File,
    offset: usize,
    nbytes: usize,
    flags: SyncFileRangeFlags,
) -> Result<(), SystemError> {
    let inode = file.inode();
    let file_type = inode.metadata()?.file_type;
    if !matches!(
        file_type,
        FileType::File | FileType::BlockDevice | FileType::Dir | FileType::SymLink
    ) {
        return Err(SystemError::ESPIPE);
    }

    let endbyte = if nbytes == 0 {
        usize::MAX
    } else {
        offset.checked_add(nbytes).ok_or(SystemError::EINVAL)? - 1
    };
    let start_index = offset >> MMArch::PAGE_SHIFT;
    let end_index = endbyte >> MMArch::PAGE_SHIFT;

    let Some(page_cache) = inode.page_cache() else {
        return Ok(());
    };
    let manager = page_cache.manager();

    if flags.contains(SyncFileRangeFlags::WAIT_BEFORE) {
        manager.wait_writeback_range(start_index, end_index)?;
        file.check_and_advance_wb_error(&page_cache)?;
    }
    if flags.contains(SyncFileRangeFlags::WRITE) {
        let sync_all =
            flags.contains(SyncFileRangeFlags::WAIT_BEFORE | SyncFileRangeFlags::WAIT_AFTER);
        manager.start_writeback_range(start_index, end_index, sync_all)?;
    }
    if flags.contains(SyncFileRangeFlags::WAIT_AFTER) {
        manager.wait_writeback_range(start_index, end_index)?;
        file.check_and_advance_wb_error(&page_cache)?;
    }

    Ok(())
}

syscall_table_macros::declare_syscall!(SYS_SYNC_FILE_RANGE, SysSyncFileRangeHandle);
