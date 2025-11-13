use crate::{
    arch::{interrupt::TrapFrame, ipc::signal::Signal, syscall::nr::SYS_SETITIMER},
    ipc::kill::{kill_process, kill_process_by_pcb},
    process::{ProcessControlBlock, ProcessManager},
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::{UserBufferReader, UserBufferWriter},
    },
    time::{
        syscall::{Itimerval, PosixTimeval},
        timer::{self, Jiffies, Timer, TimerFunction},
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

        let sig = match self.which {
            ITIMER_REAL => Signal::SIGALRM,
            ITIMER_VIRTUAL => Signal::SIGVTALRM,
            ITIMER_PROF => Signal::SIGPROF,
            _ => return Ok(()),
        };

        // 根据Linux行为，ITIMER_REAL (SIGALRM) 优先发送给线程组的leader（主线程）。
        // ITIMER_PROF 则会发送给进程中任何一个正在执行的线程。
        // 为了模拟 SIGALRM 的特殊行为，我们在这里进行分情况处理。
        if self.which == ITIMER_REAL {
            let _ = kill_process(pcb.raw_pid(), sig);
        } else {
            let _ = kill_process_by_pcb(pcb.clone(), sig);
        }

        // 周期性定时器，则重新启动定时器
        if self.interval.tv_sec > 0 || self.interval.tv_usec > 0 {
            let interval_duration = Duration::new(
                self.interval.tv_sec as u64,
                self.interval.tv_usec as u32 * 1000,
            );
            let interval_jiffies = <Jiffies as From<Duration>>::from(interval_duration).data();
            let expire_jiffies = timer::clock() + interval_jiffies;

            let next_helper = ItimerHelper::new(pcb.clone(), self.which, self.interval);
            let next_timer = Timer::new(next_helper, expire_jiffies);
            next_timer.activate();

            // 更新 PCB 中存储的定时器信息
            let mut itimers = pcb.itimers.lock();
            let new_itimer = crate::process::ProcessItimer {
                timer: next_timer,
                config: Itimerval {
                    it_interval: self.interval,
                    it_value: self.interval,
                },
            };
            match self.which {
                ITIMER_REAL => itimers.real = Some(new_itimer),
                ITIMER_VIRTUAL => itimers.virt = Some(new_itimer),
                ITIMER_PROF => itimers.prof = Some(new_itimer),
                _ => unreachable!(),
            }
        } else {
            // 一次性定时器，从 PCB 中清除
            let mut itimers = pcb.itimers.lock();
            match self.which {
                ITIMER_REAL => itimers.real = None,
                ITIMER_VIRTUAL => itimers.virt = None,
                ITIMER_PROF => itimers.prof = None,
                _ => unreachable!(),
            }
        }

        Ok(())
    }
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

        // TODO: ITIMER_VIRTUAL 和 ITIMER_PROF 需要基于CPU时间进行精确实现。
        // 当前的实现基于真实时间，所以暂时显式地返回未实现错误，以避免通过依赖真实时间的测试。
        if which == ITIMER_PROF || which == ITIMER_VIRTUAL {
            log::warn!(
                "ITIMER_VIRTUAL/PROF is not fully supported and uses real time, not CPU time."
            );
            // return Err(SystemError::ENOSYS);
        }

        let pcb = ProcessManager::current_pcb();
        let mut itimers = pcb.itimers.lock();
        let timer_slot = match which {
            ITIMER_REAL => &mut itimers.real,
            ITIMER_VIRTUAL => &mut itimers.virt,
            ITIMER_PROF => &mut itimers.prof,
            _ => unreachable!(),
        };

        // 处理 old_value
        if !old_value_ptr.is_null() {
            let mut old_itimer_val = Itimerval::default();
            if let Some(current_itimer) = timer_slot.as_ref() {
                old_itimer_val.it_interval = current_itimer.config.it_interval;
                let now = timer::clock();
                let expires = current_itimer.timer.inner().expire_jiffies;

                if expires > now {
                    let remaining_jiffies = Jiffies::new(expires - now);
                    let remaining_duration = Duration::from(remaining_jiffies);
                    old_itimer_val.it_value.tv_sec = remaining_duration.as_secs() as i64;
                    old_itimer_val.it_value.tv_usec = remaining_duration.subsec_micros() as i32;
                }
            }
            let mut writer = UserBufferWriter::new(old_value_ptr, size_of::<Itimerval>(), true)?;
            writer.copy_one_to_user(&old_itimer_val, 0)?;
        }

        // 设置 new_value
        if !new_value_ptr.is_null() {
            if let Some(old_itimer) = timer_slot.take() {
                old_itimer.timer.cancel();
            }

            let mut new_config = Itimerval::default();
            let reader = UserBufferReader::new(new_value_ptr, size_of::<Itimerval>(), true)?;
            reader.copy_one_from_user(&mut new_config, 0)?;

            // 只有当 new_config 的 it_value 非零时，才创建新的定时器
            if new_config.it_value.tv_sec > 0 || new_config.it_value.tv_usec > 0 {
                // 注意: ITIMER_VIRTUAL 和 ITIMER_PROF 的正确实现应基于CPU时间。
                let value_duration = Duration::new(
                    new_config.it_value.tv_sec as u64,
                    new_config.it_value.tv_usec as u32 * 1000,
                );
                let value_jiffies = <Jiffies as From<Duration>>::from(value_duration).data();
                let expire_jiffies = timer::clock() + value_jiffies;

                let helper = ItimerHelper::new(pcb.clone(), which, new_config.it_interval);
                let new_timer = Timer::new(helper, expire_jiffies);
                new_timer.activate();

                // 将新的定时器放回 timer_slot
                *timer_slot = Some(crate::process::ProcessItimer {
                    timer: new_timer,
                    config: new_config,
                });
            }
        }
        Ok(0)
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
