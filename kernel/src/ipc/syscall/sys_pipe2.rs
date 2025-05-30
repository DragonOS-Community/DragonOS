use crate::{
    arch::syscall::nr::SYS_PIPE2,
    filesystem::vfs::{
        file::{File, FileMode},
        FilePrivateData,
    },
    ipc::pipe::{LockedPipeInode, PipeFsPrivateData},
    libs::spinlock::SpinLock,
    process::ProcessManager,
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::UserBufferWriter,
    },
};
use alloc::vec::Vec;
use core::ffi::c_int;
use system_error::SystemError;

pub struct SysPipe2Handle;

// Extracted core logic for pipe2
// pub(super) makes it visible to other modules in kernel/src/ipc/syscall/
pub(super) fn do_kernel_pipe2(fd: *mut i32, flags: FileMode) -> Result<usize, SystemError> {
    if !flags
        .difference(FileMode::O_CLOEXEC | FileMode::O_NONBLOCK | FileMode::O_DIRECT)
        .is_empty()
    {
        return Err(SystemError::EINVAL);
    }

    let mut user_buffer = UserBufferWriter::new(fd, core::mem::size_of::<[c_int; 2]>(), true)?;
    let fd = user_buffer.buffer::<i32>(0)?;
    let pipe_ptr = LockedPipeInode::new();

    let mut read_file = File::new(
        pipe_ptr.clone(),
        FileMode::O_RDONLY | (flags & FileMode::O_NONBLOCK),
    )?;
    read_file.private_data = SpinLock::new(FilePrivateData::Pipefs(PipeFsPrivateData::new(
        FileMode::O_RDONLY,
    )));

    let mut write_file = File::new(
        pipe_ptr.clone(),
        FileMode::O_WRONLY | (flags & (FileMode::O_NONBLOCK | FileMode::O_DIRECT)),
    )?;
    write_file.private_data = SpinLock::new(FilePrivateData::Pipefs(PipeFsPrivateData::new(
        FileMode::O_WRONLY | (flags & (FileMode::O_NONBLOCK | FileMode::O_DIRECT)),
    )));

    if flags.contains(FileMode::O_CLOEXEC) {
        read_file.set_close_on_exec(true);
        write_file.set_close_on_exec(true);
    }
    let fd_table_ptr = ProcessManager::current_pcb().fd_table();
    let mut fd_table_guard = fd_table_ptr.write();
    let read_fd = fd_table_guard.alloc_fd(read_file, None)?;
    let write_fd = fd_table_guard.alloc_fd(write_file, None)?;

    drop(fd_table_guard);

    fd[0] = read_fd;
    fd[1] = write_fd;
    Ok(0)
}

impl SysPipe2Handle {
    #[inline(always)]
    fn pipefd(args: &[usize]) -> *mut i32 {
        args[0] as *mut c_int
    }
    #[inline(always)]
    fn flags(args: &[usize]) -> FileMode {
        FileMode::from_bits_truncate(args[1] as u32)
    }
}

impl Syscall for SysPipe2Handle {
    fn num_args(&self) -> usize {
        2 // fd_ptr, flags
    }

    fn handle(&self, args: &[usize], _from_user: bool) -> Result<usize, SystemError> {
        let fd_ptr = Self::pipefd(args);
        if fd_ptr.is_null() {
            return Err(SystemError::EFAULT);
        } else {
            let flags = FileMode::from_bits_truncate(args[1] as u32);
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
