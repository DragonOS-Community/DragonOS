use alloc::sync::Arc;
use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
#[cfg(target_arch = "x86_64")]
use crate::arch::syscall::nr::SYS_INOTIFY_INIT;
use crate::arch::syscall::nr::{SYS_INOTIFY_ADD_WATCH, SYS_INOTIFY_INIT1, SYS_INOTIFY_RM_WATCH};
use crate::filesystem::vfs::fcntl::AtFlags;
use crate::filesystem::vfs::file::{File, FileFlags};
use crate::filesystem::vfs::utils::user_path_at;
use crate::filesystem::vfs::MAX_PATHLEN;
use crate::filesystem::vfs::VFS_MAX_FOLLOW_SYMLINK_TIMES;
use crate::libs::casting::DowncastArc;
use crate::process::ProcessManager;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::vfs_check_and_clone_cstr;

use super::inode::InotifyInode;
use super::registry::{add_watch, rm_watch, InodeKey};
use super::uapi::{InotifyMask, WatchDescriptor, IN_CLOEXEC, IN_NONBLOCK};

#[cfg(target_arch = "x86_64")]
pub struct SysInotifyInit;

#[cfg(target_arch = "x86_64")]
impl Syscall for SysInotifyInit {
    fn num_args(&self) -> usize {
        0
    }

    fn handle(&self, _args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        sys_inotify_init1(0)
    }

    fn entry_format(&self, _args: &[usize]) -> alloc::vec::Vec<FormattedSyscallParam> {
        alloc::vec::Vec::new()
    }
}

pub struct SysInotifyInit1;

impl Syscall for SysInotifyInit1 {
    fn num_args(&self) -> usize {
        1
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let flags = args[0] as u32;
        sys_inotify_init1(flags)
    }

    fn entry_format(&self, args: &[usize]) -> alloc::vec::Vec<FormattedSyscallParam> {
        alloc::vec![FormattedSyscallParam::new(
            "flags",
            format!("{:#x}", args[0])
        )]
    }
}

pub struct SysInotifyAddWatch;

impl Syscall for SysInotifyAddWatch {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = args[0] as i32;
        let pathname = args[1] as *const u8;
        let mask_val = args[2] as u32;

        let mask = InotifyMask::from_bits(mask_val).ok_or(SystemError::EINVAL)?;

        if mask.is_empty() {
            return Err(SystemError::EINVAL);
        }

        if mask.contains(InotifyMask::IN_MASK_ADD) && mask.contains(InotifyMask::IN_MASK_CREATE) {
            return Err(SystemError::EINVAL);
        }

        let path = vfs_check_and_clone_cstr(pathname, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;

        let pcb = ProcessManager::current_pcb();
        let (inode_begin, remain_path) = user_path_at(&pcb, AtFlags::AT_FDCWD.bits(), &path)?;

        let follow_final = !mask.contains(InotifyMask::IN_DONT_FOLLOW);
        let target_inode = inode_begin.lookup_follow_symlink2(
            remain_path.as_str(),
            VFS_MAX_FOLLOW_SYMLINK_TIMES,
            follow_final,
        )?;

        if mask.contains(InotifyMask::IN_ONLYDIR)
            && target_inode.metadata()?.file_type != crate::filesystem::vfs::FileType::Dir
        {
            return Err(SystemError::ENOTDIR);
        }

        let binding = pcb.fd_table();
        let fd_table_guard = binding.read();
        let file = fd_table_guard
            .get_file_by_fd(fd)
            .ok_or(SystemError::EBADF)?;
        let inode = file.inode();
        drop(fd_table_guard);

        // Reconstruct Arc by cloning from file inode.
        let ino_arc = inode
            .clone()
            .downcast_arc::<InotifyInode>()
            .ok_or(SystemError::EINVAL)?;

        let md = target_inode.metadata()?;
        let wd = add_watch(&ino_arc, InodeKey::new(md.dev_id, md.inode_id.data()), mask)?;

        Ok(wd.0 as usize)
    }

    fn entry_format(&self, args: &[usize]) -> alloc::vec::Vec<FormattedSyscallParam> {
        alloc::vec![
            FormattedSyscallParam::new("fd", format!("{}", args[0] as i32)),
            FormattedSyscallParam::new("pathname", format!("{:#x}", args[1])),
            FormattedSyscallParam::new("mask", format!("{:#x}", args[2] as u32)),
        ]
    }
}

pub struct SysInotifyRmWatch;

impl Syscall for SysInotifyRmWatch {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = args[0] as i32;
        let wd = WatchDescriptor(args[1] as i32);

        let pcb = ProcessManager::current_pcb();
        let binding = pcb.fd_table();
        let fd_table_guard = binding.read();
        let file = fd_table_guard
            .get_file_by_fd(fd)
            .ok_or(SystemError::EBADF)?;
        let inode = file.inode();
        drop(fd_table_guard);

        let ino_arc = inode
            .clone()
            .downcast_arc::<InotifyInode>()
            .ok_or(SystemError::EINVAL)?;

        rm_watch(&ino_arc, wd)?;
        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> alloc::vec::Vec<FormattedSyscallParam> {
        alloc::vec![
            FormattedSyscallParam::new("fd", format!("{}", args[0] as i32)),
            FormattedSyscallParam::new("wd", format!("{}", args[1] as i32)),
        ]
    }
}

fn sys_inotify_init1(flags: u32) -> Result<usize, SystemError> {
    // Validate flags: only IN_CLOEXEC/IN_NONBLOCK are accepted.
    if (flags & !(IN_CLOEXEC | IN_NONBLOCK)) != 0 {
        return Err(SystemError::EINVAL);
    }

    let nonblock = (flags & IN_NONBLOCK) != 0;
    let inode = Arc::new(InotifyInode::new(nonblock));

    let mut file_flags = FileFlags::O_RDONLY;
    if (flags & IN_CLOEXEC) != 0 {
        file_flags |= FileFlags::O_CLOEXEC;
    }
    if nonblock {
        file_flags |= FileFlags::O_NONBLOCK;
    }

    let file = File::new(inode, file_flags)?;
    let binding = ProcessManager::current_pcb().fd_table();
    let mut fd_table_guard = binding.write();
    let fd = fd_table_guard.alloc_fd(file, None).map(|x| x as usize);
    fd
}

#[cfg(target_arch = "x86_64")]
syscall_table_macros::declare_syscall!(SYS_INOTIFY_INIT, SysInotifyInit);
syscall_table_macros::declare_syscall!(SYS_INOTIFY_INIT1, SysInotifyInit1);
syscall_table_macros::declare_syscall!(SYS_INOTIFY_ADD_WATCH, SysInotifyAddWatch);
syscall_table_macros::declare_syscall!(SYS_INOTIFY_RM_WATCH, SysInotifyRmWatch);
