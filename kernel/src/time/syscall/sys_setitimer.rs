use crate::{
    arch::{interrupt::TrapFrame, ipc::signal::Signal, syscall::nr::SYS_SETITIMER},
    ipc::kill::kill_process,
    process::{ProcessControlBlock, ProcessManager},
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::{UserBufferReader, UserBufferWriter},
    },
    time::{
        ns_to_timeval,
        syscall::{Itimerval, PosixTimeval},
        timer::{self, Jiffies, Timer, TimerFunction},
        timeval_to_ns,
    },
};
use alloc::{
    boxed::Box,
    string::ToString,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{mem::size_of, time::Duration};
use system_error::SystemError;

const ITIMER_REAL: i32 = 0;
const ITIMER_VIRTUAL: i32 = 1;
const ITIMER_PROF: i32 = 2;

#[derive(Debug)]
struct ItimerHelper {
    target_pcb: Weak<ProcessControlBlock>,
    which: i32,
    interval: PosixTimeval,
}

impl ItimerHelper {
    fn new(target_pcb: Arc<ProcessControlBlock>, which: i32, interval: PosixTimeval) -> Box<Self> {
        Box::new(Self {
            target_pcb: Arc::downgrade(&target_pcb),
            which,
            interval,
        })
    }
}

impl TimerFunction for ItimerHelper {
    fn run(&mut self) -> Result<(), SystemError> {
        let pcb = match self.target_pcb.upgrade() {
            Some(pcb) => pcb,
            None => return Ok(()), // 进程已退出
        };

        // 根据Linux行为，ITIMER_REAL (SIGALRM) 优先发送给线程组的leader。
        let thread_group_leader = pcb.threads_read_irqsave().group_leader();
        if let Some(leader) = thread_group_leader {
            let _ = kill_process(leader.raw_pid(), Signal::SIGALRM);
        } else {
            let _ = kill_process(pcb.raw_pid(), Signal::SIGALRM);
        }

        // 周期性定时器，则重新启动定时器
        if self.interval.tv_sec > 0 || self.interval.tv_usec > 0 {
            let interval_duration = Duration::new(
                self.interval.tv_sec as u64,
                self.interval.tv_usec as u32 * 1000,
            );
            let expire_jiffies =
                timer::clock() + <Jiffies as From<Duration>>::from(interval_duration).data();

            let next_helper = ItimerHelper::new(pcb.clone(), self.which, self.interval);
            let next_timer = Timer::new(next_helper, expire_jiffies);
            next_timer.activate();

            let mut itimers = pcb.itimers_irqsave();
            let new_itimer = crate::process::ProcessItimer {
                timer: next_timer,
                config: Itimerval {
                    it_interval: self.interval,
                    it_value: self.interval, // 下一次的值就是间隔
                },
            };
            itimers.real = Some(new_itimer);
        } else {
            // 一次性定时器，从 PCB 中清除
            let mut itimers = pcb.itimers_irqsave();
            itimers.real = None;
        }
        Ok(())
    }
}

/// 处理 ITIMER_REAL
fn handle_itimer_real(
    pcb: Arc<ProcessControlBlock>,
    new_value_ptr: *const Itimerval,
    old_value_ptr: *mut Itimerval,
) -> Result<usize, SystemError> {
    let mut itimers = pcb.itimers_irqsave();

    // old_value: 获取旧值
    if !old_value_ptr.is_null() {
        let mut old_itv = Itimerval::default();
        if let Some(current_itimer) = itimers.real.as_ref() {
            old_itv.it_interval = current_itimer.config.it_interval;
            let now = timer::clock();
            let expires = current_itimer.timer.inner().expire_jiffies;
            if expires > now {
                let remaining_jiffies = Jiffies::new(expires - now);
                let remaining_duration = Duration::from(remaining_jiffies);
                old_itv.it_value.tv_sec = remaining_duration.as_secs() as i64;
                old_itv.it_value.tv_usec = remaining_duration.subsec_micros() as i32;
            }
        }
        let mut writer = UserBufferWriter::new(old_value_ptr, size_of::<Itimerval>(), true)?;
        writer.copy_one_to_user(&old_itv, 0)?;
    }

    // new_value: 设置新值
    if !new_value_ptr.is_null() {
        // 先取消旧的真实时间定时器
        if let Some(old_itimer) = itimers.real.take() {
            old_itimer.timer.cancel();
        }

        let mut new_config = Itimerval::default();
        let reader = UserBufferReader::new(new_value_ptr, size_of::<Itimerval>(), true)?;
        reader.copy_one_from_user(&mut new_config, 0)?;

        // 如果 it_value 非零，创建并激活新的真实时间定时器
        if new_config.it_value.tv_sec > 0 || new_config.it_value.tv_usec > 0 {
            let value_duration = Duration::new(
                new_config.it_value.tv_sec as u64,
                new_config.it_value.tv_usec as u32 * 1000,
            );
            let expire_jiffies =
                timer::clock() + <Jiffies as From<Duration>>::from(value_duration).data();

            let helper = ItimerHelper::new(pcb.clone(), ITIMER_REAL, new_config.it_interval);
            let new_timer = Timer::new(helper, expire_jiffies);
            new_timer.activate();

            // 将新的定时器放回 itimers
            itimers.real = Some(crate::process::ProcessItimer {
                timer: new_timer,
                config: new_config,
            });
        }
    }
    Ok(0)
}

/// 处理 ITIMER_VIRTUAL 和 ITIMER_PROF
fn handle_itimer_cpu(
    pcb: Arc<ProcessControlBlock>,
    which: i32,
    new_value_ptr: *const Itimerval,
    old_value_ptr: *mut Itimerval,
) -> Result<usize, SystemError> {
    let mut itimers = pcb.itimers_irqsave();
    let cpu_timer_slot = if which == ITIMER_VIRTUAL {
        &mut itimers.virt
    } else {
        &mut itimers.prof
    };

    // 获取旧值
    if !old_value_ptr.is_null() {
        let mut old_itv = Itimerval::default();
        if cpu_timer_slot.is_active {
            old_itv.it_value = ns_to_timeval(cpu_timer_slot.value);
            old_itv.it_interval = ns_to_timeval(cpu_timer_slot.interval);
        }
        let mut writer = UserBufferWriter::new(old_value_ptr, size_of::<Itimerval>(), true)?;
        writer.copy_one_to_user(&old_itv, 0)?;
    }

    // 设置新值
    if !new_value_ptr.is_null() {
        let mut new_config = Itimerval::default();
        let reader = UserBufferReader::new(new_value_ptr, size_of::<Itimerval>(), true)?;
        reader.copy_one_from_user(&mut new_config, 0)?;

        let value_ns = timeval_to_ns(&new_config.it_value);
        if value_ns > 0 {
            // 激活或重置定时器
            cpu_timer_slot.value = value_ns;
            cpu_timer_slot.interval = timeval_to_ns(&new_config.it_interval);
            cpu_timer_slot.is_active = true;
        } else {
            // value 为 0，表示取消定时器
            cpu_timer_slot.is_active = false;
        }
    }
    Ok(0)
}

pub struct SysSetitimerHandle;

impl Syscall for SysSetitimerHandle {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let which = args[0] as i32;
        let new_value_ptr = args[1] as *const Itimerval;
        let old_value_ptr = args[2] as *mut Itimerval;

        if which < ITIMER_REAL || which > ITIMER_PROF {
            return Err(SystemError::EINVAL);
        }

        let pcb = ProcessManager::current_pcb();

        // 根据定时器类型，分派到不同的处理函数
        match which {
            ITIMER_REAL => handle_itimer_real(pcb, new_value_ptr, old_value_ptr),
            ITIMER_VIRTUAL | ITIMER_PROF => {
                handle_itimer_cpu(pcb, which, new_value_ptr, old_value_ptr)
            }
            _ => unreachable!(),
        }
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("which", (args[0] as i32).to_string()),
            FormattedSyscallParam::new("new_value", format!("{:#x}", args[1])),
            FormattedSyscallParam::new("old_value", format!("{:#x}", args[2])),
        ]
    }
}

// 注册系统调用
syscall_table_macros::declare_syscall!(SYS_SETITIMER, SysSetitimerHandle);
