//! Linux `openat2(2)` user ABI and strict request validation.

use alloc::{string::ToString, vec::Vec};
use core::mem::size_of;
use system_error::SystemError;

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_OPENAT2, MMArch},
    filesystem::vfs::{
        file::FileFlags,
        open::do_sys_openat2,
        syscall::{OpenHow, PosixOpenHow},
        utils::OpenHowResolve,
        InodeMode, MAX_PATHLEN,
    },
    mm::MemoryManagementArch,
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::{vfs_check_and_clone_cstr, UserBufferReader},
    },
};

pub struct SysOpenat2Handle;

impl Syscall for SysOpenat2Handle {
    fn num_args(&self) -> usize {
        4
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let size = args[3];
        if size < size_of::<PosixOpenHow>() {
            return Err(SystemError::EINVAL);
        }
        if size > MMArch::PAGE_SIZE {
            return Err(SystemError::E2BIG);
        }

        let how = read_open_how(args[2] as *const u8, size)?;
        let how = validate_open_how(how)?;
        let path = vfs_check_and_clone_cstr(args[1] as *const u8, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        do_sys_openat2(args[0] as i32, &path, how)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("dirfd", (args[0] as i32).to_string()),
            FormattedSyscallParam::new("pathname", format!("{:#x}", args[1])),
            FormattedSyscallParam::new("how", format!("{:#x}", args[2])),
            FormattedSyscallParam::new("size", args[3].to_string()),
        ]
    }
}

fn read_open_how(ptr: *const u8, size: usize) -> Result<PosixOpenHow, SystemError> {
    let reader = UserBufferReader::new(ptr, size, true)?;
    // copy_struct_from_user checks unknown trailing bytes before copying known
    // fields. Never dereference a userspace pointer without exception protection.
    let mut offset = size_of::<PosixOpenHow>();
    let mut tail = [0u8; 64];
    while offset < size {
        let len = (size - offset).min(tail.len());
        reader.copy_from_user_protected(&mut tail[..len], offset)?;
        if tail[..len].iter().any(|byte| *byte != 0) {
            return Err(SystemError::E2BIG);
        }
        offset += len;
    }
    reader.read_one_from_user(0)
}

fn validate_open_how(raw: PosixOpenHow) -> Result<OpenHow, SystemError> {
    // Linux build_open_flags strips this internal fanotify bit before strict
    // userspace flag validation; do not pass it into the VFS file flags.
    const FMODE_NONOTIFY: u64 = 0x0400_0000;
    let mut flags = FileFlags::from_bits(
        u32::try_from(raw.flags & !FMODE_NONOTIFY).map_err(|_| SystemError::EINVAL)?,
    )
    .ok_or(SystemError::EINVAL)?;
    let resolve = OpenHowResolve::from_bits(raw.resolve).ok_or(SystemError::EINVAL)?;
    if resolve.contains(OpenHowResolve::RESOLVE_BENEATH)
        && resolve.contains(OpenHowResolve::RESOLVE_IN_ROOT)
    {
        return Err(SystemError::EINVAL);
    }

    let creates = flags.intersects(FileFlags::O_CREAT | FileFlags::__O_TMPFILE);
    if (creates && raw.mode & !(InodeMode::S_IALLUGO.bits() as u64) != 0)
        || (!creates && raw.mode != 0)
    {
        return Err(SystemError::EINVAL);
    }
    if flags.contains(FileFlags::O_CREAT | FileFlags::O_DIRECTORY) {
        return Err(SystemError::EINVAL);
    }
    if flags.contains(FileFlags::__O_TMPFILE)
        && (!flags.contains(FileFlags::O_DIRECTORY) || flags.access_flags() == FileFlags::O_RDONLY)
    {
        return Err(SystemError::EINVAL);
    }
    if flags.contains(FileFlags::O_PATH) && !FileFlags::O_PATH_FLAGS.contains(flags) {
        return Err(SystemError::EINVAL);
    }
    if flags.contains(FileFlags::__O_SYNC) {
        flags.insert(FileFlags::O_DSYNC);
    }
    if resolve.contains(OpenHowResolve::RESOLVE_CACHED)
        && flags.intersects(FileFlags::O_TRUNC | FileFlags::O_CREAT | FileFlags::__O_TMPFILE)
    {
        return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
    }
    if !flags.contains(FileFlags::O_PATH) {
        flags.insert(FileFlags::O_LARGEFILE);
    }

    let mode = InodeMode::from_bits(raw.mode as u32).ok_or(SystemError::EINVAL)?;
    Ok(OpenHow::new(flags, mode, resolve))
}

syscall_table_macros::declare_syscall!(SYS_OPENAT2, SysOpenat2Handle);
