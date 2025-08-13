use alloc::string::ToString;

use alloc::vec::Vec;

use crate::{
    arch::{
        interrupt::TrapFrame,
        syscall::nr::{SYS_SYNC, SYS_SYNCFS},
    },
    mm::page::page_reclaimer_lock_irqsave,
    syscall::table::{FormattedSyscallParam, Syscall},
};

/// sync() causes all pending modifications to filesystem metadata and
/// cached file data to be written to the underlying filesystems.
///
/// See https://man7.org/linux/man-pages/man2/sync.2.html
pub struct SysSyncHandle;

impl Syscall for SysSyncHandle {
    fn num_args(&self) -> usize {
        0
    }

    fn handle(
        &self,
        _args: &[usize],
        _frame: &mut TrapFrame,
    ) -> Result<usize, system_error::SystemError> {
        page_reclaimer_lock_irqsave().flush_dirty_pages();
        Ok(0)
    }

    fn entry_format(&self, _args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "No arguments",
            "sync()".to_string(),
        )]
    }
}

syscall_table_macros::declare_syscall!(SYS_SYNC, SysSyncHandle);

/// syncfs() is like sync(), but synchronizes just the filesystem
/// containing file referred to by the open file descriptor fd.
pub struct SysSyncFsHandle;

impl Syscall for SysSyncFsHandle {
    fn num_args(&self) -> usize {
        1
    }

    fn handle(
        &self,
        _args: &[usize],
        _frame: &mut TrapFrame,
    ) -> Result<usize, system_error::SystemError> {
        // TODO: now, we ignore the fd and sync all filesystems.
        // In the future, we should sync only the filesystem of the given fd.
        page_reclaimer_lock_irqsave().flush_dirty_pages();
        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new("fd", format!("{}", args[0]))]
    }
}

syscall_table_macros::declare_syscall!(SYS_SYNCFS, SysSyncHandle);
