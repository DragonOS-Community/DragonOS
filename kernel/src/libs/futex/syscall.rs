use system_error::SystemError;

use crate::{
    libs::futex::futex::RobustListHead,
    mm::{verify_area, VirtAddr},
    syscall::Syscall,
    time::PosixTimeSpec,
};

use super::{constant::*, futex::Futex};

impl Syscall {
    pub fn do_futex(
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

    pub fn set_robust_list(head_uaddr: VirtAddr, len: usize) -> Result<usize, SystemError> {
        //判断用户空间地址的合法性
        verify_area(head_uaddr, core::mem::size_of::<u32>())?;

        let ret = RobustListHead::set_robust_list(head_uaddr, len);
        // log::debug!(
        //     "set_robust_list: pid: {} head_uaddr={:?}",
        //     crate::process::ProcessManager::current_pid(),
        //     head_uaddr
        // );
        return ret;
    }

    pub fn get_robust_list(
        pid: usize,
        head_uaddr: VirtAddr,
        len_ptr_uaddr: VirtAddr,
    ) -> Result<usize, SystemError> {
        //判断用户空间地址的合法性
        verify_area(head_uaddr, core::mem::size_of::<u32>())?;
        verify_area(len_ptr_uaddr, core::mem::size_of::<u32>())?;

        let ret = RobustListHead::get_robust_list(pid, head_uaddr, len_ptr_uaddr);
        return ret;
    }
}
