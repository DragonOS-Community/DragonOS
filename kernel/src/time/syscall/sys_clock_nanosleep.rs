use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_CLOCK_NANOSLEEP;
use crate::ipc::signal::{RestartBlock, RestartBlockData};
use crate::mm::VirtAddr;
use crate::process::ProcessManager;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::{UserBufferReader, UserBufferWriter};
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
        super::posix_clock::posix_clock_now(clockid)
    }

    #[inline]
    fn is_valid_timespec(ts: &PosixTimeSpec) -> bool {
        ts.is_valid_timeout()
    }

    #[inline]
    fn to_ns(ts: &PosixTimeSpec) -> u64 {
        // 这里已经保证了 tv_sec/tv_nsec 非负且 tv_nsec < 1e9
        (ts.tv_sec as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(ts.tv_nsec as u64)
    }

    #[inline]
    fn calc_remaining(deadline: &PosixTimeSpec, now: &PosixTimeSpec) -> PosixTimeSpec {
        deadline.saturating_sub_timespec(now)
    }

    #[inline]
    fn add_timespec(a: &PosixTimeSpec, b: &PosixTimeSpec) -> PosixTimeSpec {
        a.saturating_add_ktime(b)
    }

    fn do_wait_until(deadline: &PosixTimeSpec, clockid: PosixClockID) -> Result<(), SystemError> {
        match clockid {
            ProcessCPUTimeID => {
                let current = ProcessManager::current_pcb();
                let leader = if current.is_thread_group_leader() {
                    current
                } else {
                    current
                        .threads_read_irqsave()
                        .group_leader()
                        .unwrap_or_else(ProcessManager::current_pcb)
                };

                let deadline_ns = Self::to_ns(deadline);
                leader.cputime_wait_queue().wait_event_interruptible(
                    || leader.process_cputime_ns() >= deadline_ns,
                    None::<fn()>,
                )?;
                Ok(())
            }
            ThreadCPUTimeID => {
                let pcb = ProcessManager::current_pcb();
                let deadline_ns = Self::to_ns(deadline);
                pcb.cputime_wait_queue().wait_event_interruptible(
                    || pcb.thread_cputime_ns() >= deadline_ns,
                    None::<fn()>,
                )?;
                Ok(())
            }
            _ => {
                let now = Self::ktime_now(clockid);
                let remain = Self::calc_remaining(deadline, &now);
                if remain.tv_sec == 0 && remain.tv_nsec == 0 {
                    return Ok(());
                }
                nanosleep(remain).map(|_| ())
            }
        }
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
            Realtime | Monotonic | Boottime | ProcessCPUTimeID => {}
            ThreadCPUTimeID => return Err(SystemError::EINVAL),
            _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
        }
        let flags = Self::flags(args);
        // Linux 6.6 only interprets TIMER_ABSTIME here; unknown bits are
        // ignored by the POSIX clock nanosleep implementations.
        let is_abstime = (flags & 0x01) != 0; // TIMER_ABSTIME = 1

        // 读取 rqtp
        let rq_reader = UserBufferReader::new(
            Self::rqtp(args),
            core::mem::size_of::<PosixTimeSpec>(),
            true,
        )?;
        let rq_user = rq_reader.read_one_from_user::<PosixTimeSpec>(0)?;
        if !Self::is_valid_timespec(&rq_user) {
            return Err(SystemError::EINVAL);
        }
        let rq = PosixTimeSpec {
            tv_sec: rq_user.tv_sec,
            tv_nsec: rq_user.tv_nsec,
        };

        let rmtp = if !Self::rmtp(args).is_null() && !is_abstime {
            Some(VirtAddr::new(Self::rmtp(args) as usize))
        } else {
            None
        };

        // 计算 deadline
        let deadline_clockid = if !is_abstime && matches!(clockid, Realtime) {
            Monotonic
        } else {
            clockid
        };
        let deadline: PosixTimeSpec = if is_abstime {
            PosixTimeSpec {
                tv_sec: rq.tv_sec,
                tv_nsec: rq.tv_nsec,
            }
        } else {
            let now = Self::ktime_now(deadline_clockid);
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
        let wait_res = Self::do_wait_until(&deadline, deadline_clockid);
        match wait_res {
            Ok(()) => {
                // log::debug!(
                //     "clock_nanosleep: completed normally (flags={} is_abs={})",
                //     flags,
                //     is_abstime
                // );
                return Ok(0);
            }
            Err(SystemError::ERESTARTSYS) => {
                // 信号打断
                if is_abstime {
                    // 绝对睡眠：返回 -ERESTARTNOHAND，不写 rmtp
                    // log::debug!("clock_nanosleep: ABS interrupted -> ERESTARTNOHAND");
                    return Err(SystemError::ERESTARTNOHAND);
                } else {
                    // 相对睡眠：写回剩余时间，并设置restart block
                    if let Some(rmtp) = rmtp {
                        let now = Self::ktime_now(deadline_clockid);
                        let remain = Self::calc_remaining(&deadline, &now);
                        // log::debug!(
                        //     "clock_nanosleep: REL interrupted -> write rem {{sec={}, nsec={}}}",
                        //     remain.tv_sec,
                        //     remain.tv_nsec
                        // );
                        let mut writer = UserBufferWriter::new(
                            rmtp.as_ptr::<PosixTimeSpec>(),
                            core::mem::size_of::<PosixTimeSpec>(),
                            true,
                        )?;
                        writer.copy_one_to_user(&remain, 0)?;
                    }
                    // 设置重启函数
                    let data = RestartBlockData::Nanosleep {
                        deadline,
                        clockid: deadline_clockid,
                        rmtp,
                    };
                    // log::debug!(
                    //     "clock_nanosleep: set restart block and return ERESTART_RESTARTBLOCK"
                    // );
                    let rb = RestartBlock::new(&crate::ipc::signal::RestartFnNanosleep, data);
                    return ProcessManager::current_pcb().set_restart_fn(Some(rb));
                }
            }
            Err(error) => return Err(error),
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
