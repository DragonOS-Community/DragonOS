use system_error::SystemError;

use crate::libs::futex::{constant::*, futex::Futex};

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_FUTEX},
    mm::{verify_area, VirtAddr},
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::UserBufferReader,
    },
    time::PosixTimeSpec,
};
use alloc::string::ToString;
use alloc::vec::Vec;

/// System call handler for the `futex` syscall
///
/// This handler implements the `Syscall` trait to provide functionality for fast userspace mutexes.
/// Futex provides a mechanism for userspace programs to implement efficient synchronization primitives.
pub struct SysFutexHandle;

impl Syscall for SysFutexHandle {
    /// Returns the number of arguments expected by the `futex` syscall
    fn num_args(&self) -> usize {
        6
    }

    /// Handles the `futex` system call
    ///
    /// Provides fast userspace mutex functionality including wait, wake, requeue, and wake_op operations.
    ///
    /// # Arguments
    /// * `args` - Array containing:
    ///   - args[0]: uaddr - Pointer to the futex word (*u32)
    ///   - args[1]: futex_op - Futex operation (u32)
    ///   - args[2]: val - Value for the operation (u32)
    ///   - args[3]: utime - Timeout pointer (*const PosixTimeSpec) or 0
    ///   - args[4]: uaddr2 - Second futex word pointer (*u32)
    ///   - args[5]: val3 - Third value for the operation (u32)
    /// * `frame` - Trap frame containing execution context
    ///
    /// # Returns
    /// * `Ok(usize)` - Number of processes woken up or operation result
    /// * `Err(SystemError)` - Error code if operation fails
    fn handle(&self, args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let uaddr = Self::uaddr(args);
        let operation = Self::operation(args)?;
        let val = Self::val(args);
        let utime = Self::utime(args);
        let uaddr2 = Self::uaddr2(args);
        let val3 = Self::val3(args);

        let mut timespec = None;
        if utime != 0 {
            let reader = UserBufferReader::new(
                utime as *const PosixTimeSpec,
                core::mem::size_of::<PosixTimeSpec>(),
                frame.is_from_user(),
            )?;

            timespec = Some(*reader.read_one_from_user::<PosixTimeSpec>(0)?);
        }

        do_futex(uaddr, operation, val, timespec, uaddr2, utime as u32, val3)
    }

    /// Formats the syscall parameters for display/debug purposes
    ///
    /// # Arguments
    /// * `args` - The raw syscall arguments
    ///
    /// # Returns
    /// Vector of formatted parameters with descriptive names
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("uaddr", format!("{:#x}", Self::uaddr(args).data())),
            FormattedSyscallParam::new("futex_op", format!("{:#x}", Self::operation_raw(args))),
            FormattedSyscallParam::new("val", Self::val(args).to_string()),
            FormattedSyscallParam::new("utime", format!("{:#x}", Self::utime(args))),
            FormattedSyscallParam::new("uaddr2", format!("{:#x}", Self::uaddr2(args).data())),
            FormattedSyscallParam::new("val3", Self::val3(args).to_string()),
        ]
    }
}

impl SysFutexHandle {
    /// Extracts the futex word address from syscall arguments
    fn uaddr(args: &[usize]) -> VirtAddr {
        VirtAddr::new(args[0])
    }

    /// Extracts the futex operation from syscall arguments
    fn operation(args: &[usize]) -> Result<FutexFlag, SystemError> {
        FutexFlag::from_bits(args[1] as u32).ok_or(SystemError::ENOSYS)
    }

    /// Extracts the raw futex operation from syscall arguments (for formatting)
    fn operation_raw(args: &[usize]) -> u32 {
        args[1] as u32
    }

    /// Extracts the value from syscall arguments
    fn val(args: &[usize]) -> u32 {
        args[2] as u32
    }

    /// Extracts the timeout pointer from syscall arguments
    fn utime(args: &[usize]) -> usize {
        args[3]
    }

    /// Extracts the second futex word address from syscall arguments
    fn uaddr2(args: &[usize]) -> VirtAddr {
        VirtAddr::new(args[4])
    }

    /// Extracts the third value from syscall arguments
    fn val3(args: &[usize]) -> u32 {
        args[5] as u32
    }
}

syscall_table_macros::declare_syscall!(SYS_FUTEX, SysFutexHandle);

pub(super) fn do_futex(
    uaddr: VirtAddr,
    operation: FutexFlag,
    val: u32,
    timeout: Option<PosixTimeSpec>,
    uaddr2: VirtAddr,
    val2: u32,
    val3: u32,
) -> Result<usize, SystemError> {
    verify_area(uaddr, core::mem::size_of::<u32>())?;
    verify_area(uaddr2, core::mem::size_of::<u32>())?;
    let cmd = FutexArg::from_bits(operation.bits() & FutexFlag::FUTEX_CMD_MASK.bits())
        .ok_or(SystemError::ENOSYS)?;

    let mut flags = FutexFlag::FLAGS_MATCH_NONE;

    if !operation.contains(FutexFlag::FUTEX_PRIVATE_FLAG) {
        flags.insert(FutexFlag::FLAGS_SHARED);
    }

    if operation.contains(FutexFlag::FUTEX_CLOCK_REALTIME) {
        flags.insert(FutexFlag::FLAGS_CLOCKRT);
        if cmd != FutexArg::FUTEX_WAIT_BITSET
            && cmd != FutexArg::FUTEX_WAIT_REQUEUE_PI
            && cmd != FutexArg::FUTEX_LOCK_PI2
        {
            return Err(SystemError::ENOSYS);
        }
    }

    match cmd {
        FutexArg::FUTEX_WAIT => {
            return Futex::futex_wait(uaddr, flags, val, timeout, FUTEX_BITSET_MATCH_ANY);
        }
        FutexArg::FUTEX_WAIT_BITSET => {
            // Linux 语义：WAIT_BITSET 的超时为绝对时间（clock_nanosleep 风格）。
            // 这里将绝对截止时间转换为相对剩余时间，past 则立即 ETIMEDOUT。
            let adjusted_timeout = if let Some(deadline) = timeout {
                // 校验 timespec 合法性
                if deadline.tv_nsec < 0 || deadline.tv_nsec >= 1_000_000_000 {
                    return Err(SystemError::EINVAL);
                }

                // 选择时钟：若带 FUTEX_CLOCK_REALTIME 则使用 realtime；否则使用 monotonic（当前实现等价）
                let now = crate::time::timekeeping::getnstimeofday();

                // 计算剩余时间 = deadline - now，若 <=0 则立即超时
                let mut sec = deadline.tv_sec - now.tv_sec;
                let mut nsec = deadline.tv_nsec - now.tv_nsec;
                if nsec < 0 {
                    nsec += 1_000_000_000;
                    sec -= 1;
                }
                if sec < 0 || (sec == 0 && nsec == 0) {
                    return Err(SystemError::ETIMEDOUT);
                }

                Some(PosixTimeSpec {
                    tv_sec: sec,
                    tv_nsec: nsec,
                })
            } else {
                None
            };

            return Futex::futex_wait(uaddr, flags, val, adjusted_timeout, val3);
        }
        FutexArg::FUTEX_WAKE => {
            return Futex::futex_wake(uaddr, flags, val, FUTEX_BITSET_MATCH_ANY);
        }
        FutexArg::FUTEX_WAKE_BITSET => {
            return Futex::futex_wake(uaddr, flags, val, val3);
        }
        FutexArg::FUTEX_REQUEUE => {
            return Futex::futex_requeue(
                uaddr,
                flags,
                uaddr2,
                val as i32,
                val2 as i32,
                None,
                false,
            );
        }
        FutexArg::FUTEX_CMP_REQUEUE => {
            return Futex::futex_requeue(
                uaddr,
                flags,
                uaddr2,
                val as i32,
                val2 as i32,
                Some(val3),
                false,
            );
        }
        FutexArg::FUTEX_WAKE_OP => {
            return Futex::futex_wake_op(
                uaddr,
                flags,
                uaddr2,
                val as i32,
                val2 as i32,
                val3 as i32,
            );
        }
        FutexArg::FUTEX_LOCK_PI => {
            todo!()
        }
        FutexArg::FUTEX_LOCK_PI2 => {
            todo!()
        }
        FutexArg::FUTEX_UNLOCK_PI => {
            todo!()
        }
        FutexArg::FUTEX_TRYLOCK_PI => {
            todo!()
        }
        FutexArg::FUTEX_WAIT_REQUEUE_PI => {
            todo!()
        }
        FutexArg::FUTEX_CMP_REQUEUE_PI => {
            todo!()
        }
        _ => {
            return Err(SystemError::ENOSYS);
        }
    }
}
