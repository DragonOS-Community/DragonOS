use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_CLOCK_NANOSLEEP;
use crate::ipc::signal::{RestartBlock, RestartBlockData};
use crate::process::ProcessManager;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::{UserBufferReader, UserBufferWriter};
use crate::time::timekeeping::getnstimeofday;
use crate::time::{sleep::nanosleep, PosixTimeSpec};
use alloc::vec::Vec;
use system_error::SystemError;

use super::{PosixClockID, PosixClockID::*};

pub struct SysClockNanosleep;

impl SysClockNanosleep {
    fn which_clock(args: &[usize]) -> i32 {
        args[0] as i32
    }
    fn flags(args: &[usize]) -> i32 {
        args[1] as i32
    }
    fn rqtp(args: &[usize]) -> *const PosixTimeSpec {
        args[2] as *const PosixTimeSpec
    }
    fn rmtp(args: &[usize]) -> *mut PosixTimeSpec {
        args[3] as *mut PosixTimeSpec
    }

    #[inline]
    fn ktime_now(clockid: PosixClockID) -> PosixTimeSpec {
        // 暂时使用 realtime 近似；后续区分 monotonic/boottime
        // - Realtime：使用 getnstimeofday()
        // - Monotonic/Boottime：暂与 Realtime 等价（后续引入真正单调/启动时钟）
        match clockid {
            Realtime => getnstimeofday(),
            Monotonic | Boottime => getnstimeofday(),
            _ => getnstimeofday(),
        }
    }

    #[inline]
    fn is_valid_timespec(ts: &PosixTimeSpec) -> bool {
        ts.tv_nsec >= 0 && ts.tv_nsec < 1_000_000_000
    }

    #[inline]
    fn calc_remaining(deadline: &PosixTimeSpec, now: &PosixTimeSpec) -> PosixTimeSpec {
        let mut sec = deadline.tv_sec - now.tv_sec;
        let mut nsec = deadline.tv_nsec - now.tv_nsec;
        if nsec < 0 {
            sec -= 1;
            nsec += 1_000_000_000;
        }
        if sec < 0 {
            return PosixTimeSpec {
                tv_sec: 0,
                tv_nsec: 0,
            };
        }
        PosixTimeSpec {
            tv_sec: sec,
            tv_nsec: nsec,
        }
    }

    #[inline]
    fn add_timespec(a: &PosixTimeSpec, b: &PosixTimeSpec) -> PosixTimeSpec {
        let mut sec = a.tv_sec + b.tv_sec;
        let mut nsec = a.tv_nsec + b.tv_nsec;
        if nsec >= 1_000_000_000 {
            sec += 1;
            nsec -= 1_000_000_000;
        }
        PosixTimeSpec {
            tv_sec: sec,
            tv_nsec: nsec,
        }
    }

    fn do_wait_until(deadline: &PosixTimeSpec, clockid: PosixClockID) -> Result<(), SystemError> {
        let now = Self::ktime_now(clockid);
        let remain = Self::calc_remaining(deadline, &now);
        if remain.tv_sec == 0 && remain.tv_nsec == 0 {
            return Ok(());
        }
        nanosleep(remain).map(|_| ())
    }
}

impl Syscall for SysClockNanosleep {
    fn num_args(&self) -> usize {
        4
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        // 解析/校验参数
        let clockid = PosixClockID::try_from(Self::which_clock(args))?;
        match clockid {
            Realtime | Monotonic | Boottime => {}
            _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
        }
        let flags = Self::flags(args);
        let is_abstime = (flags & 0x01) != 0; // TIMER_ABSTIME = 1

        // 读取 rqtp
        let rq_reader = UserBufferReader::new(
            Self::rqtp(args),
            core::mem::size_of::<PosixTimeSpec>(),
            true,
        )?;
        let rq_user = rq_reader.read_one_from_user::<PosixTimeSpec>(0)?;
        if !Self::is_valid_timespec(rq_user) {
            return Err(SystemError::EINVAL);
        }
        let rq = PosixTimeSpec {
            tv_sec: rq_user.tv_sec,
            tv_nsec: rq_user.tv_nsec,
        };

        let rmtp_ptr = Self::rmtp(args);
        let mut rmtp_writer = if !rmtp_ptr.is_null() && !is_abstime {
            Some(UserBufferWriter::new(
                rmtp_ptr,
                core::mem::size_of::<PosixTimeSpec>(),
                true,
            )?)
        } else {
            None
        };

        // 计算 deadline
        let deadline: PosixTimeSpec = if is_abstime {
            PosixTimeSpec {
                tv_sec: rq.tv_sec,
                tv_nsec: rq.tv_nsec,
            }
        } else {
            let now = Self::ktime_now(clockid);
            Self::add_timespec(&now, &rq)
        };

        // 立即到期检查（ABS）
        if is_abstime {
            let now = Self::ktime_now(clockid);
            let remain = Self::calc_remaining(&deadline, &now);
            if remain.tv_sec == 0 && remain.tv_nsec == 0 {
                return Ok(0);
            }
        }

        // 等待
        let wait_res = Self::do_wait_until(&deadline, clockid);
        match wait_res {
            Ok(()) => {
                // log::debug!(
                //     "clock_nanosleep: completed normally (flags={} is_abs={})",
                //     flags,
                //     is_abstime
                // );
                return Ok(0);
            }
            Err(_e) => {
                // 信号打断
                if is_abstime {
                    // 绝对睡眠：返回 -ERESTARTNOHAND，不写 rmtp
                    // log::debug!("clock_nanosleep: ABS interrupted -> ERESTARTNOHAND");
                    return Err(SystemError::ERESTARTNOHAND);
                } else {
                    // 相对睡眠：写回剩余时间，并设置restart block
                    if let Some(ref mut w) = rmtp_writer {
                        let now = Self::ktime_now(clockid);
                        let remain = Self::calc_remaining(&deadline, &now);
                        // log::debug!(
                        //     "clock_nanosleep: REL interrupted -> write rem {{sec={}, nsec={}}}",
                        //     remain.tv_sec,
                        //     remain.tv_nsec
                        // );
                        w.copy_one_to_user(&remain, 0)?;
                    }
                    // 设置重启函数
                    let data = RestartBlockData::Nanosleep { deadline, clockid };
                    // log::debug!(
                    //     "clock_nanosleep: set restart block and return ERESTART_RESTARTBLOCK"
                    // );
                    let rb = RestartBlock::new(&crate::ipc::signal::RestartFnNanosleep, data);
                    return ProcessManager::current_pcb().set_restart_fn(Some(rb));
                }
            }
        }
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("which_clock", format!("{}", Self::which_clock(args))),
            FormattedSyscallParam::new("flags", format!("{:#x}", Self::flags(args))),
            FormattedSyscallParam::new("rqtp", format!("{:#x}", Self::rqtp(args) as usize)),
            FormattedSyscallParam::new("rmtp", format!("{:#x}", Self::rmtp(args) as usize)),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_CLOCK_NANOSLEEP, SysClockNanosleep);
