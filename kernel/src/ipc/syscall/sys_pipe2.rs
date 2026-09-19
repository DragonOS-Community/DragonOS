use crate::arch::interrupt::TrapFrame;
use crate::{
    arch::syscall::nr::SYS_PIPE2,
    filesystem::vfs::file::{File, FileFlags},
    ipc::pipe::LockedPipeInode,
    process::ProcessManager,
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::UserBufferWriter,
    },
};
use alloc::{sync::Arc, vec::Vec};
use core::ffi::c_int;
use system_error::SystemError;
pub struct SysPipe2Handle;

// Extracted core logic for pipe2
// pub(super) makes it visible to other modules in kernel/src/ipc/syscall/
pub(super) fn do_kernel_pipe2(fd: *mut i32, flags: FileFlags) -> Result<usize, SystemError> {
    if !flags
        .difference(FileFlags::O_CLOEXEC | FileFlags::O_NONBLOCK | FileFlags::O_DIRECT)
        .is_empty()
    {
        return Err(SystemError::EINVAL);
    }

    let mut user_buffer = UserBufferWriter::new(fd, core::mem::size_of::<[c_int; 2]>(), true)?;
    let fd = user_buffer.buffer::<i32>(0)?;
    let pipe_ptr = LockedPipeInode::new();

    let read_file = File::new(
        pipe_ptr.clone(),
        FileFlags::O_RDONLY | (flags & FileFlags::O_NONBLOCK),
    )?;

    let write_file = File::new(
        pipe_ptr.clone(),
        FileFlags::O_WRONLY | (flags & (FileFlags::O_NONBLOCK | FileFlags::O_DIRECT)),
    )?;
    let read_file = Arc::try_new(read_file).map_err(|_| SystemError::ENOMEM)?;
    let write_file = Arc::try_new(write_file).map_err(|_| SystemError::ENOMEM)?;

    let cloexec = flags.contains(FileFlags::O_CLOEXEC);
    let current = ProcessManager::current_pcb();
    let reservation = current
        .fd_table()
        .reserve::<2>(current.nofile_soft_limit(), 0, cloexec)?;
    fd[0] = reservation.fd(0);
    fd[1] = reservation.fd(1);
    reservation.install_arc_pair(read_file, write_file)?;
    Ok(0)
}

impl SysPipe2Handle {
    #[inline(always)]
    fn pipefd(args: &[usize]) -> *mut i32 {
        args[0] as *mut c_int
    }
    #[inline(always)]
    fn flags(args: &[usize]) -> FileFlags {
        FileFlags::from_bits_truncate(args[1] as u32)
    }
}

impl Syscall for SysPipe2Handle {
    fn num_args(&self) -> usize {
        2 // fd_ptr, flags
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd_ptr = Self::pipefd(args);
        if fd_ptr.is_null() {
            return Err(SystemError::EFAULT);
        } else {
            let flags = FileFlags::from_bits_truncate(args[1] as u32);
            do_kernel_pipe2(fd_ptr, flags)
        }
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        let fd_ptr = Self::pipefd(args);
        vec![
            FormattedSyscallParam::new("fd_ptr", format!("{}", fd_ptr as usize)), // Format pointer as hex
            FormattedSyscallParam::new("flags", format!("{}", Self::flags(args).bits())),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_PIPE2, SysPipe2Handle);
