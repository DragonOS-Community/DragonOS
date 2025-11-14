use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_GETITIMER},
    process::ProcessManager,
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::UserBufferWriter,
    },
    time::{
        ns_to_timeval,
        syscall::{Itimerval, PosixSusecondsT, PosixTimeT},
        timer::{self, Jiffies},
    },
};
use alloc::{string::ToString, vec::Vec};
use core::mem::size_of;
use system_error::SystemError;

const ITIMER_REAL: i32 = 0;
const ITIMER_VIRTUAL: i32 = 1;
const ITIMER_PROF: i32 = 2;

pub struct SysGetitimerHandle;

impl Syscall for SysGetitimerHandle {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let which = args[0] as i32;
        let curr_value_ptr = args[1] as *mut Itimerval;

        if which < ITIMER_REAL || which > ITIMER_PROF {
            return Err(SystemError::EINVAL);
        }

        let pcb = ProcessManager::current_pcb();
        let itimers = pcb.itimers_irqsave(); // 假设使用Mutex
        let mut itv = Itimerval::default();

        match which {
            ITIMER_REAL => {
                // 读取真实时间定时器
                if let Some(current_itimer) = itimers.real.as_ref() {
                    itv.it_interval = current_itimer.config.it_interval;
                    let now = timer::clock();
                    let expires = current_itimer.timer.inner().expire_jiffies;

                    if expires > now {
                        let remaining_jiffies = Jiffies::new(expires - now);
                        let remaining_duration = core::time::Duration::from(remaining_jiffies);
                        itv.it_value.tv_sec = remaining_duration.as_secs() as PosixTimeT;
                        itv.it_value.tv_usec =
                            remaining_duration.subsec_micros() as PosixSusecondsT;
                    }
                }
            }
            ITIMER_VIRTUAL => {
                if itimers.virt.is_active {
                    itv.it_value = ns_to_timeval(itimers.virt.value);
                    itv.it_interval = ns_to_timeval(itimers.virt.interval);
                }
            }
            ITIMER_PROF => {
                if itimers.prof.is_active {
                    itv.it_value = ns_to_timeval(itimers.prof.value);
                    itv.it_interval = ns_to_timeval(itimers.prof.interval);
                }
            }
            _ => unreachable!(),
        }

        if !curr_value_ptr.is_null() {
            let mut writer = UserBufferWriter::new(curr_value_ptr, size_of::<Itimerval>(), true)?;
            writer.copy_one_to_user(&itv, 0)?;
        }

        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("which", (args[0] as i32).to_string()),
            FormattedSyscallParam::new("value", format!("{:#x}", args[1])),
        ]
    }
}

// 注册系统调用
syscall_table_macros::declare_syscall!(SYS_GETITIMER, SysGetitimerHandle);
