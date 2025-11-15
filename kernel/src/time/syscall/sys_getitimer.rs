use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_GETITIMER},
    process::ProcessManager,
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::UserBufferWriter,
    },
    time::{
        syscall::{ItimerType, Itimerval, PosixSusecondsT, PosixTimeT, PosixTimeval},
        timer::{self, Jiffies},
    },
};
use alloc::{string::ToString, vec::Vec};
use core::mem::size_of;
use system_error::SystemError;

pub struct SysGetitimerHandle;

impl SysGetitimerHandle {
    fn which(args: &[usize]) -> Result<ItimerType, SystemError> {
        ItimerType::try_from(args[0] as i32)
    }

    fn curr_value_ptr(args: &[usize]) -> *mut Itimerval {
        args[1] as *mut Itimerval
    }
}

impl Syscall for SysGetitimerHandle {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let which = Self::which(args)?;
        let curr_value_ptr = Self::curr_value_ptr(args);

        let pcb = ProcessManager::current_pcb();
        let itimers = pcb.itimers_irqsave();
        let mut itv = Itimerval::default();

        match which {
            ItimerType::Real => {
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
            ItimerType::Virtual => {
                if itimers.virt.is_active {
                    itv.it_value = PosixTimeval::from_ns(itimers.virt.value);
                    itv.it_interval = PosixTimeval::from_ns(itimers.virt.interval);
                }
            }
            ItimerType::Prof => {
                if itimers.prof.is_active {
                    itv.it_value = PosixTimeval::from_ns(itimers.prof.value);
                    itv.it_interval = PosixTimeval::from_ns(itimers.prof.interval);
                }
            }
        }

        if !curr_value_ptr.is_null() {
            let mut writer = UserBufferWriter::new(curr_value_ptr, size_of::<Itimerval>(), true)?;
            writer.copy_one_to_user(&itv, 0)?;
        }

        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        let which_str = match ItimerType::try_from(args[0] as i32) {
            Ok(ItimerType::Real) => "ITIMER_REAL".to_string(),
            Ok(ItimerType::Virtual) => "ITIMER_VIRTUAL".to_string(),
            Ok(ItimerType::Prof) => "ITIMER_PROF".to_string(),
            Err(_) => format!("Invalid({})", args[0]),
        };
        vec![
            FormattedSyscallParam::new("which", which_str),
            FormattedSyscallParam::new("value", format!("{:#x}", args[1])),
        ]
    }
}

// 注册系统调用
syscall_table_macros::declare_syscall!(SYS_GETITIMER, SysGetitimerHandle);
