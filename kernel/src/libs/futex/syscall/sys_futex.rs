use core::sync::atomic::{compiler_fence, Ordering};

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
        let operation = Self::operation(args);
        let val = Self::val(args);
        // 第4个参数：不同操作下语义不同（可能是 timeout 指针、val2、op 等）
        let arg4 = Self::utime(args);
        let uaddr2 = Self::uaddr2(args);
        let val3 = Self::val3(args);

        // 决定是否将第4参解释为超时指针（WAIT* 系列）或数值 val2（REQUEUE/WAKE_OP 等）
        let cmd = FutexArg::from_bits(operation & FutexFlag::FUTEX_CMD_MASK.bits())
            .ok_or(SystemError::ENOSYS)?;

        let (timespec, val2): (Option<PosixTimeSpec>, u32) = match cmd {
            // 与 Linux 语义一致：WAIT 使用相对超时；WAIT_BITSET/LOCK_PI2/WAIT_REQUEUE_PI 使用绝对时间（若带 CLOCKRT）
            FutexArg::FUTEX_WAIT
            | FutexArg::FUTEX_WAIT_BITSET
            | FutexArg::FUTEX_WAIT_REQUEUE_PI
            | FutexArg::FUTEX_LOCK_PI2 => {
                if arg4 != 0 {
                    let reader = UserBufferReader::new(
                        arg4 as *const PosixTimeSpec,
                        core::mem::size_of::<PosixTimeSpec>(),
                        frame.is_from_user(),
                    )?;
                    (Some(*reader.read_one_from_user::<PosixTimeSpec>(0)?), 0)
                } else {
                    (None, 0)
                }
            }
            _ => {
                // 其他操作中，第4参为数值（如 REQUEUE 的 nr_requeue、WAKE_OP 的 nr_wake2 等）
                (None, arg4 as u32)
            }
        };

        do_futex(uaddr, operation, val, timespec, uaddr2, val2, val3)
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
    fn operation(args: &[usize]) -> u32 {
        args[1] as u32
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
    operation: u32,
    val: u32,
    timeout: Option<PosixTimeSpec>,
    uaddr2: VirtAddr,
    val2: u32,
    val3: u32,
) -> Result<usize, SystemError> {
    defer::defer!({
        compiler_fence(Ordering::SeqCst);
    });

    let cmd = FutexArg::from_bits(operation & FutexFlag::FUTEX_CMD_MASK.bits())
        .ok_or(SystemError::ENOSYS)?;

    // 仅在需要 uaddr2 的操作中校验它
    match cmd {
        FutexArg::FUTEX_REQUEUE
        | FutexArg::FUTEX_CMP_REQUEUE
        | FutexArg::FUTEX_WAKE_OP
        | FutexArg::FUTEX_WAIT_REQUEUE_PI
        | FutexArg::FUTEX_CMP_REQUEUE_PI => {
            verify_area(uaddr2, core::mem::size_of::<u32>())?;
        }
        _ => {}
    }

    let mut flags = FutexFlag::FLAGS_MATCH_NONE;

    if (operation & FutexFlag::FUTEX_PRIVATE_FLAG.bits()) == 0 {
        flags.insert(FutexFlag::FLAGS_SHARED);
    }

    if (operation & FutexFlag::FUTEX_CLOCK_REALTIME.bits()) != 0 {
        flags.insert(FutexFlag::FLAGS_CLOCKRT);
        if cmd != FutexArg::FUTEX_WAIT_BITSET
            && cmd != FutexArg::FUTEX_WAIT_REQUEUE_PI
            && cmd != FutexArg::FUTEX_LOCK_PI2
        {
            return Err(SystemError::ENOSYS);
        }
    }

    // 对于 FUTEX_WAKE_OP 的私有 futex，允许 uaddr 为 NULL（Linux 兼容行为）。
    // 仅在不满足该例外时才校验 uaddr。
    let skip_uaddr_check = cmd == FutexArg::FUTEX_WAKE_OP
        && (operation & FutexFlag::FUTEX_PRIVATE_FLAG.bits()) != 0
        && uaddr.data() == 0;

    if !skip_uaddr_check {
        verify_area(uaddr, core::mem::size_of::<u32>())?;
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
            return Futex::futex_lock_pi(uaddr, flags, timeout);
        }
        FutexArg::FUTEX_LOCK_PI2 => {
            // FUTEX_LOCK_PI2 与 FUTEX_LOCK_PI 行为相同，只是支持 FUTEX_CLOCK_REALTIME
            // 超时处理已在上层完成，这里直接调用 futex_lock_pi
            return Futex::futex_lock_pi(uaddr, flags, timeout);
        }
        FutexArg::FUTEX_UNLOCK_PI => {
            return Futex::futex_unlock_pi(uaddr, flags);
        }
        FutexArg::FUTEX_TRYLOCK_PI => {
            return Futex::futex_trylock_pi(uaddr, flags);
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
