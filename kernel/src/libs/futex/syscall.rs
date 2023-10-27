use crate::{
    mm::VirtAddr,
    syscall::{Syscall, SystemError},
    time::TimeSpec,
};

use super::{constant::*, futex::Futex};

impl Syscall {
    pub fn do_futex(
        uaddr: VirtAddr,
        operation: FutexFlag,
        val: u32,
        timeout: Option<TimeSpec>,
        uaddr2: VirtAddr,
        val2: u32,
        val3: u32,
    ) -> Result<usize, SystemError> {
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
                return Futex::futex_wait(uaddr, flags, val, timeout, val3);
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
}
