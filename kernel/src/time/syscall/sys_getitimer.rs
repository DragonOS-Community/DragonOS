use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_GETITIMER},
    process::ProcessManager,
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::UserBufferWriter,
    },
    time::{
        syscall::Itimerval,
        timer::{self, Jiffies},
    },
};
use alloc::{string::ToString, vec::Vec};
use core::{mem::size_of, time::Duration};
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
        let itimers = pcb.itimers.lock();

        let timer_slot = match which {
            ITIMER_REAL => &itimers.real,
            // TODO: 未完全实现，应使用进程的用户态CPU时间记账。
            ITIMER_VIRTUAL => &itimers.virt,
            // TODO: 未完全实现，应使用进程的用户态+内核态CPU时间记账。
            ITIMER_PROF => &itimers.prof,
            _ => unreachable!(),
        };

        let mut itv = Itimerval::default();
        if let Some(current_itimer) = timer_slot.as_ref() {
            itv.it_interval = current_itimer.config.it_interval;
            // TODO: timer::clock()获取的是系统启动以来的真实时间，但对于 ITIMER_VIRTUAL 和 ITIMER_PROF，这里应该获取进程对应的CPU时间。
            let now = timer::clock();
            let expires = current_itimer.timer.inner().expire_jiffies;

            if expires > now {
                let remaining_jiffies = Jiffies::new(expires - now);
                let remaining_duration = Duration::from(remaining_jiffies);
                itv.it_value.tv_sec = remaining_duration.as_secs() as i64;
                itv.it_value.tv_usec = remaining_duration.subsec_micros() as i32;
            }
            // 如果已过期，it_value 默认是 0，这是正确的
        }

        // 写入用户空间
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
