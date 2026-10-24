use crate::arch::syscall::nr::SYS_UTIME;
use crate::{
    arch::interrupt::TrapFrame,
    filesystem::vfs::syscall::UtimensFlags,
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::UserBufferReader,
    },
    time::PosixTimeSpec,
};
use alloc::vec::Vec;
use system_error::SystemError;

/// Linux `struct utimbuf`：两个 64 位 time_t（秒级精度）。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct UtimeBuf {
    actime: i64,
    modtime: i64,
}

/// `utime(2)`: 以秒级精度设置文件的访问/修改时间。
///
/// 语义与 Linux 对齐（fs/utimes.c）：
/// - `times == NULL`：两者均设为当前时间（等价 UTIME_NOW）；
/// - 跟随符号链接；
/// - 复用 `utimensat` 的路径解析与权限检查路径。
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
            let buf = reader.read_from_user::<UtimeBuf>(0)?[0];
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

        super::utimensat::do_sys_utimensat(
            crate::filesystem::vfs::fcntl::AtFlags::AT_FDCWD.bits(),
            filename,
            times
                .as_ref()
                .map(|t| t.as_ptr())
                .unwrap_or(core::ptr::null()),
            UtimensFlags::empty().bits(),
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
