//! System call handler for opening files.

use system_error::SystemError;

use crate::arch::syscall::nr::SYS_FSTAT;
use crate::filesystem::vfs::stat::PosixKstat;
use crate::filesystem::vfs::{FileType, ModeType};
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::UserBufferWriter;

use alloc::string::ToString;
use alloc::vec::Vec;

pub struct SysFstatHandle;

impl Syscall for SysFstatHandle {
    /// Returns the number of arguments this syscall takes.
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _from_user: bool) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let usr_kstat = Self::usr_kstat(args);

        let mut writer = UserBufferWriter::new(usr_kstat, size_of::<PosixKstat>(), true)?;
        let kstat = do_fstat(fd)?;

        writer.copy_one_to_user(&kstat, 0)?;
        return Ok(0);
    }

    /// Formats the syscall arguments for display/debugging purposes.
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fd", Self::fd(args).to_string()),
            FormattedSyscallParam::new("statbuf", format!("{:#x}", Self::usr_kstat(args) as usize)),
        ]
    }
}

impl SysFstatHandle {
    /// Extracts the fd argument from syscall parameters.
    fn fd(args: &[usize]) -> i32 {
        args[0] as i32
    }

    /// Extracts the usr_kstat argument from syscall parameters.
    fn usr_kstat(args: &[usize]) -> *mut PosixKstat {
        args[1] as *mut PosixKstat
    }
}

syscall_table_macros::declare_syscall!(SYS_FSTAT, SysFstatHandle);

pub(super) fn do_fstat(fd: i32) -> Result<PosixKstat, SystemError> {
    let binding = ProcessManager::current_pcb().fd_table();
    let fd_table_guard = binding.read();
    let file = fd_table_guard
        .get_file_by_fd(fd)
        .ok_or(SystemError::EBADF)?;
    // drop guard 以避免无法调度的问题
    drop(fd_table_guard);

    let mut kstat = PosixKstat::new();
    // 获取文件信息
    let metadata = file.metadata()?;
    kstat.size = metadata.size;
    kstat.dev_id = metadata.dev_id as u64;
    kstat.inode = metadata.inode_id.into() as u64;
    kstat.blcok_size = metadata.blk_size as i64;
    kstat.blocks = metadata.blocks as u64;

    kstat.atime.tv_sec = metadata.atime.tv_sec;
    kstat.atime.tv_nsec = metadata.atime.tv_nsec;
    kstat.mtime.tv_sec = metadata.mtime.tv_sec;
    kstat.mtime.tv_nsec = metadata.mtime.tv_nsec;
    kstat.ctime.tv_sec = metadata.ctime.tv_sec;
    kstat.ctime.tv_nsec = metadata.ctime.tv_nsec;

    kstat.nlink = metadata.nlinks as u64;
    kstat.uid = metadata.uid as i32;
    kstat.gid = metadata.gid as i32;
    kstat.rdev = metadata.raw_dev.data() as i64;
    kstat.mode = metadata.mode;
    match file.file_type() {
        FileType::File => kstat.mode.insert(ModeType::S_IFREG),
        FileType::Dir => kstat.mode.insert(ModeType::S_IFDIR),
        FileType::BlockDevice => kstat.mode.insert(ModeType::S_IFBLK),
        FileType::CharDevice => kstat.mode.insert(ModeType::S_IFCHR),
        FileType::SymLink => kstat.mode.insert(ModeType::S_IFLNK),
        FileType::Socket => kstat.mode.insert(ModeType::S_IFSOCK),
        FileType::Pipe => kstat.mode.insert(ModeType::S_IFIFO),
        FileType::KvmDevice => kstat.mode.insert(ModeType::S_IFCHR),
        FileType::FramebufferDevice => kstat.mode.insert(ModeType::S_IFCHR),
    }

    return Ok(kstat);
}
