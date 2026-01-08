use alloc::sync::Arc;
use alloc::vec::Vec;
use system_error::SystemError;

use crate::arch::syscall::nr::SYS_TEE;
use crate::filesystem::vfs::{file::File, syscall::SpliceFlags, FileType};
use crate::ipc::pipe::LockedPipeInode;
use crate::libs::casting::DowncastArc;
use crate::process::ProcessManager;
use crate::syscall::table::Syscall;

/// See <https://man7.org/linux/man-pages/man2/tee.2.html>
///
/// tee() duplicates data from one pipe to another without consuming it.
pub struct SysTeeHandle;

impl Syscall for SysTeeHandle {
    fn num_args(&self) -> usize {
        4
    }

    fn handle(
        &self,
        args: &[usize],
        _frame: &mut crate::arch::interrupt::TrapFrame,
    ) -> Result<usize, SystemError> {
        let fd_in = args[0] as i32;
        let fd_out = args[1] as i32;
        let len = args[2];
        let flags = args[3] as u32;

        if len == 0 {
            return Ok(0);
        }

        let mut splice_flags = SpliceFlags::from_bits(flags).ok_or(SystemError::EINVAL)?;

        let (file_in, file_out) = {
            let binding = ProcessManager::current_pcb().fd_table();
            let fd_table_guard = binding.read();

            let file_in = fd_table_guard
                .get_file_by_fd(fd_in)
                .ok_or(SystemError::EBADF)?;
            let file_out = fd_table_guard
                .get_file_by_fd(fd_out)
                .ok_or(SystemError::EBADF)?;
            (file_in.clone(), file_out.clone())
        };

        if !is_pipe(&file_in) || !is_pipe(&file_out) {
            return Err(SystemError::EINVAL);
        }

        // Linux: inherit O_NONBLOCK from file descriptors.
        if file_in
            .flags()
            .contains(crate::filesystem::vfs::file::FileFlags::O_NONBLOCK)
            || file_out
                .flags()
                .contains(crate::filesystem::vfs::file::FileFlags::O_NONBLOCK)
        {
            splice_flags.insert(SpliceFlags::SPLICE_F_NONBLOCK);
        }

        // Same pipe is invalid.
        if Arc::ptr_eq(&file_in.inode(), &file_out.inode()) {
            return Err(SystemError::EINVAL);
        }

        let in_pipe = file_in
            .inode()
            .downcast_arc::<LockedPipeInode>()
            .ok_or(SystemError::EBADF)?;
        let out_pipe = file_out
            .inode()
            .downcast_arc::<LockedPipeInode>()
            .ok_or(SystemError::EBADF)?;

        let copied = in_pipe.tee_to(&out_pipe, len, splice_flags)?;
        Ok(copied)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<crate::syscall::table::FormattedSyscallParam> {
        vec![
            crate::syscall::table::FormattedSyscallParam::new("fd_in", format!("{:#x}", args[0])),
            crate::syscall::table::FormattedSyscallParam::new("fd_out", format!("{:#x}", args[1])),
            crate::syscall::table::FormattedSyscallParam::new("len", format!("{:#x}", args[2])),
            crate::syscall::table::FormattedSyscallParam::new("flags", format!("{:#x}", args[3])),
        ]
    }
}

fn is_pipe(file: &File) -> bool {
    file.inode()
        .metadata()
        .map(|md| md.file_type == FileType::Pipe)
        .unwrap_or(false)
}

syscall_table_macros::declare_syscall!(SYS_TEE, SysTeeHandle);
