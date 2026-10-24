use crate::arch::syscall::nr::SYS_UTIME;
use crate::{
    arch::interrupt::TrapFrame,
    filesystem::vfs::{open::do_utimensat, syscall::UtimensFlags, MAX_PATHLEN},
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::{vfs_check_and_clone_cstr, UserBufferReader},
    },
    time::PosixTimeSpec,
};
use alloc::vec::Vec;
use system_error::SystemError;

/// Linux `struct utimbuf`: two 64-bit time_t fields with second precision.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct UtimeBuf {
    actime: i64,
    modtime: i64,
}

/// Set file access and modification timestamps with second precision.
///
/// Match Linux utime(2) semantics (fs/utimes.c):
/// - A null times pointer sets both timestamps to the current time (UTIME_NOW).
/// - Follow symbolic links.
/// - Reuse the utimensat path resolution and permission checks.
pub struct SysUtimeHandle;

impl Syscall for SysUtimeHandle {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let filename = Self::filename(args);
        let times = Self::times(args);

        if filename.is_null() {
            return Err(SystemError::EFAULT);
        }

        let times = if times.is_null() {
            None
        } else {
            let reader = UserBufferReader::new(times, core::mem::size_of::<UtimeBuf>(), true)?;
            let buf = reader.read_one_from_user::<UtimeBuf>(0)?;
            Some([
                PosixTimeSpec {
                    tv_sec: buf.actime,
                    tv_nsec: 0,
                },
                PosixTimeSpec {
                    tv_sec: buf.modtime,
                    tv_nsec: 0,
                },
            ])
        };

        let pathname = vfs_check_and_clone_cstr(filename, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        // The converted timestamps are kernel values, not a user buffer.
        do_utimensat(
            crate::filesystem::vfs::fcntl::AtFlags::AT_FDCWD.bits(),
            Some(pathname),
            times,
            UtimensFlags::empty(),
        )
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("filename", format!("{:#x}", Self::filename(args) as usize)),
            FormattedSyscallParam::new("times", format!("{:#x}", Self::times(args) as usize)),
        ]
    }
}

impl SysUtimeHandle {
    fn filename(args: &[usize]) -> *const u8 {
        args[0] as *const u8
    }

    fn times(args: &[usize]) -> *const UtimeBuf {
        args[1] as *const UtimeBuf
    }
}

syscall_table_macros::declare_syscall!(SYS_UTIME, SysUtimeHandle);
