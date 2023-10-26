use crate::{
    syscall::{Syscall, SystemError},
    time::TimeSpec,
};

use super::{constant::*, futex::Futex};

impl Syscall {
    pub fn do_futex(
        uaddr: *const u32,
        operation: u32,
        val: u32,
        timeout: *const TimeSpec,
        uaddr2: *const u32,
        val2: u32,
        val3: u32,
    ) -> Result<usize, SystemError> {
        let cmd = operation & FUTEX_CMD_MASK;

        let mut flags = 0;

        if operation & FUTEX_PRIVATE_FLAG == 0 {
            flags = FLAGS_SHARED;
        }

        if operation & FUTEX_CLOCK_REALTIME != 0 {
            flags = FLAGS_CLOCKRT;
            if cmd != FUTEX_WAIT_BITSET && cmd != FUTEX_WAIT_REQUEUE_PI && cmd != FUTEX_LOCK_PI2 {
                return Err(SystemError::ENOSYS);
            }
        }

        match cmd {
            FUTEX_WAIT => {
                return Futex::futex_wait(uaddr, flags, val, timeout, FUTEX_BITSET_MATCH_ANY);
            }
            FUTEX_WAIT_BITSET => {
                return Futex::futex_wait(uaddr, flags, val, timeout, val3);
            }
            FUTEX_WAKE => {
                return Futex::futex_wake(uaddr, flags, val, FUTEX_BITSET_MATCH_ANY);
            }
            FUTEX_WAKE_BITSET => {
                return Futex::futex_wake(uaddr, flags, val, val3);
            }
            FUTEX_REQUEUE => {
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
            FUTEX_CMP_REQUEUE => {
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
            FUTEX_WAKE_OP => {
                return Futex::futex_wake_op(
                    uaddr,
                    flags,
                    uaddr2,
                    val as i32,
                    val2 as i32,
                    val3 as i32,
                );
            }
            FUTEX_LOCK_PI => {
                todo!()
            }
            FUTEX_LOCK_PI2 => {
                todo!()
            }
            FUTEX_UNLOCK_PI => {
                todo!()
            }
            FUTEX_TRYLOCK_PI => {
                todo!()
            }
            FUTEX_WAIT_REQUEUE_PI => {
                todo!()
            }
            FUTEX_CMP_REQUEUE_PI => {
                todo!()
            }
            _ => {
                return Err(SystemError::ENOSYS);
            }
        }
    }
}
