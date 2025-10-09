use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_ALARM;
use crate::process::{timer::AlarmTimer, ProcessManager};
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use alloc::vec::Vec;
use core::time::Duration;
use system_error::SystemError;

pub struct SysAlarm;

impl SysAlarm {
    fn expired_second(args: &[usize]) -> u32 {
        args[0] as u32
    }
}

impl Syscall for SysAlarm {
    fn num_args(&self) -> usize {
        1
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let expired_second = Self::expired_second(args);

        //初始化second
        let second = Duration::from_secs(expired_second as u64);
        let pcb = ProcessManager::current_pcb();
        let mut pcb_alarm = pcb.alarm_timer_irqsave();
        let alarm = pcb_alarm.as_ref();
        //alarm第一次调用
        if alarm.is_none() {
            //注册alarm定时器
            let pid = ProcessManager::current_pid();
            let new_alarm = Some(AlarmTimer::alarm_timer_init(pid, 0));
            *pcb_alarm = new_alarm;
            drop(pcb_alarm);
            return Ok(0);
        }
        //查询上一个alarm的剩余时间和重新注册alarm
        let alarmtimer = alarm.unwrap();
        let remain = alarmtimer.remain();
        if second.is_zero() {
            alarmtimer.cancel();
        }
        if !alarmtimer.timeout() {
            alarmtimer.cancel();
        }
        let pid = ProcessManager::current_pid();
        let new_alarm = Some(AlarmTimer::alarm_timer_init(pid, second.as_secs()));
        *pcb_alarm = new_alarm;
        drop(pcb_alarm);
        return Ok(remain.as_secs() as usize);
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "expired_second",
            format!("{}", Self::expired_second(args)),
        )]
    }
}

#[cfg(target_arch = "x86_64")]
syscall_table_macros::declare_syscall!(SYS_ALARM, SysAlarm);
