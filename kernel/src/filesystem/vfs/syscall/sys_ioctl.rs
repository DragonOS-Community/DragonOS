//! System call handler for ioctls.

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_IOCTL;
use crate::filesystem::vfs::fasync::FAsyncItem;
use crate::filesystem::vfs::file::File;
use crate::filesystem::vfs::file::FileFlags;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use system_error::SystemError;

use alloc::string::ToString;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::process::{ProcessManager, RawPid};
use crate::syscall::user_access::{UserBufferReader, UserBufferWriter};

// 通用文件描述符ioctl命令常量
const FIONBIO: u32 = 0x5421; // Set/clear non-blocking i/o
const FIONCLEX: u32 = 0x5450; // Clear close-on-exec flag
const FIOCLEX: u32 = 0x5451; // Set close-on-exec flag
const FIOASYNC: u32 = 0x5452; // Set/clear async i/o
const FIOSETOWN: u32 = 0x8901; // Set owner (pid)
const SIOCSPGRP: u32 = 0x8902; // Set process group
const FIOGETOWN: u32 = 0x8903; // Get owner (pid)
const SIOCGPGRP: u32 = 0x8904; // Get process group

/// Handler for the `ioctl` system call.
pub struct SysIoctlHandle;

impl Syscall for SysIoctlHandle {
    /// Returns the number of arguments this syscall takes (3).
    fn num_args(&self) -> usize {
        3
    }

    /// Sends a command to the device corresponding to the file descriptor.
    ///
    /// # Arguments
    ///
    /// * `fd` - File descriptor number
    /// * `cmd` - Device-dependent request code
    ///
    /// # Returns
    ///
    /// * `Ok(usize)` - On success, returns 0
    /// * `Err(SystemError)` - On failure, returns a POSIX error code
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let cmd = Self::cmd(args);
        let data = Self::data(args);

        let binding = ProcessManager::current_pcb().fd_table();
        let fd_table_guard = binding.read();

        let file = fd_table_guard
            .get_file_by_fd(fd as i32)
            .ok_or(SystemError::EBADF)?;

        // drop guard 以避免无法调度的问题
        drop(fd_table_guard);

        // 检查文件是否以 O_PATH 打开，如果是则返回 EBADF
        if file.flags().contains(FileFlags::O_PATH) {
            return Err(SystemError::EBADF);
        }

        // 处理通用文件描述符ioctl命令
        match cmd {
            FIONBIO => {
                return Self::handle_fionbio(&file, data);
            }
            FIONCLEX => {
                file.set_close_on_exec(false);
                return Ok(0);
            }
            FIOCLEX => {
                file.set_close_on_exec(true);
                return Ok(0);
            }
            FIOASYNC => {
                return Self::handle_fioasync(&file, data);
            }
            FIOSETOWN | SIOCSPGRP => {
                return Self::handle_fiosetown(&file, data);
            }
            FIOGETOWN | SIOCGPGRP => {
                return Self::handle_ownership_get(&file, data, cmd);
            }
            _ => {
                // 其他命令转发给inode处理
                let r = file
                    .inode()
                    .ioctl(cmd, data, &file.private_data.lock())
                    .map_err(|e| {
                        // 将内部错误码 ENOIOCTLCMD 转换为用户空间错误码 ENOTTY
                        if e == SystemError::ENOIOCTLCMD {
                            SystemError::ENOTTY
                        } else {
                            e
                        }
                    });
                return r;
            }
        }
    }

    /// Formats the syscall arguments for display/debugging purposes.
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fd", Self::fd(args).to_string()),
            FormattedSyscallParam::new("cmd", format!("{:#x}", Self::cmd(args))),
            FormattedSyscallParam::new("data", format!("{:#x}", Self::data(args))),
        ]
    }
}

impl SysIoctlHandle {
    /// Extracts the file descriptor argument from syscall parameters.
    fn fd(args: &[usize]) -> usize {
        args[0]
    }

    /// Extracts the command argument from syscall parameters.
    fn cmd(args: &[usize]) -> u32 {
        args[1] as u32
    }

    /// Extracts the data argument from syscall parameters.
    fn data(args: &[usize]) -> usize {
        args[2]
    }

    /// Handle FIONBIO command: set/clear non-blocking I/O
    fn handle_fionbio(file: &File, data: usize) -> Result<usize, SystemError> {
        // 检查data指针是否为null
        if data == 0 {
            return Err(SystemError::EFAULT);
        }

        // 从用户空间读取int值
        let user_reader =
            UserBufferReader::new(data as *const i32, core::mem::size_of::<i32>(), true)?;
        let value = user_reader.buffer_protected(0)?.read_one::<i32>(0)?;

        // 获取当前文件标志
        let mut flags = file.flags();
        if value != 0 {
            flags.insert(FileFlags::O_NONBLOCK);
        } else {
            flags.remove(FileFlags::O_NONBLOCK);
        }

        // 更新文件标志
        file.set_flags(flags)?;
        Ok(0)
    }

    /// Handle FIOASYNC command: set/clear asynchronous I/O
    fn handle_fioasync(file: &Arc<File>, data: usize) -> Result<usize, SystemError> {
        // 检查data指针是否为null
        if data == 0 {
            return Err(SystemError::EFAULT);
        }

        // 从用户空间读取int值
        let user_reader =
            UserBufferReader::new(data as *const i32, core::mem::size_of::<i32>(), true)?;
        let value = user_reader.buffer_protected(0)?.read_one::<i32>(0)?;

        // 获取当前文件模式
        let mut flags = file.flags();
        if value != 0 {
            flags.insert(FileFlags::FASYNC);

            // 通过 PollableInode 接口注册 FAsyncItem
            if let Ok(pollable) = file.inode().as_pollable_inode() {
                let fasync_item = Arc::new(FAsyncItem::new(Arc::downgrade(file)));
                // 忽略不支持 fasync 的 inode
                let _ = pollable.add_fasync(fasync_item, &file.private_data.lock());
            }
        } else {
            flags.remove(FileFlags::FASYNC);

            // 通过 PollableInode 接口移除 FAsyncItem
            if let Ok(pollable) = file.inode().as_pollable_inode() {
                // 忽略不支持 fasync 的 inode
                let _ = pollable.remove_fasync(&Arc::downgrade(file), &file.private_data.lock());
            }
        }

        // 更新文件标志
        file.set_flags(flags)?;
        Ok(0)
    }

    /// Handle FIOSETOWN command: set file owner for SIGIO/SIGURG signals
    fn handle_fiosetown(file: &File, data: usize) -> Result<usize, SystemError> {
        // 检查data指针是否为null
        if data == 0 {
            return Err(SystemError::EFAULT);
        }

        // 从用户空间读取pid值
        let user_reader =
            UserBufferReader::new(data as *const i32, core::mem::size_of::<i32>(), true)?;
        let pid_value = user_reader.buffer_protected(0)?.read_one::<i32>(0)?;

        // 处理pid值，逻辑与fcntl F_SETOWN相同
        let pid = pid_value.unsigned_abs();
        if pid > i32::MAX as u32 {
            return Err(SystemError::EINVAL);
        }

        let pb = if pid == 0 {
            None
        } else {
            // 注意：这里只处理进程ID，不处理进程组ID（负值）
            // 在Linux中，负值表示进程组，但DragonOS当前可能不支持
            let pb = ProcessManager::find_task_by_vpid(RawPid::from(pid as _))
                .ok_or(SystemError::ESRCH)?;
            Some(pb)
        };

        file.set_owner(pb)?;
        Ok(0)
    }

    /// Handle FIOGETOWN and SIOCGPGRP commands: get file owner/process group
    fn handle_ownership_get(file: &File, data: usize, _cmd: u32) -> Result<usize, SystemError> {
        // 检查data指针是否为null
        if data == 0 {
            return Err(SystemError::EFAULT);
        }

        // 获取所有者pid（如果没有所有者则返回0）
        let owner = file.owner().unwrap_or(RawPid::from(0));
        let pid_value: i32 = owner.data() as i32;

        // 注意：Linux中FIOGETOWN和SIOCGPGRP可能对进程组ID有不同符号处理
        // 这里简单返回pid值，与fcntl F_GETOWN保持一致
        let value_to_write = pid_value;

        // 将pid值写入用户空间
        let mut user_writer =
            UserBufferWriter::new(data as *mut i32, core::mem::size_of::<i32>(), true)?;
        user_writer
            .buffer_protected(0)?
            .write_one(0, &value_to_write)?;

        Ok(0)
    }
}

syscall_table_macros::declare_syscall!(SYS_IOCTL, SysIoctlHandle);
