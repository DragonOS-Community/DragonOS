use crate::{
    arch::{interrupt::TrapFrame, ipc::signal::Signal, syscall::nr::SYS_SETITIMER},
    ipc::kill::send_signal_to_pid,
    process::{ProcessControlBlock, ProcessItimers, ProcessManager},
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::{UserBufferReader, UserBufferWriter},
    },
    time::{
        syscall::{ItimerType, Itimerval, PosixTimeval},
        timer::{self, Jiffies, Timer, TimerFunction},
    },
};
use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{mem::size_of, time::Duration};
use system_error::SystemError;

impl ItimerType {
    /// 根据定时器类型，从内部状态获取当前的 Itimerval 值。
    fn get_current_value(&self, itimers: &ProcessItimers) -> Itimerval {
        match self {
            ItimerType::Real => {
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
                old_itv
            }
            ItimerType::Virtual | ItimerType::Prof => {
                let mut old_itv = Itimerval::default();
                let cpu_timer_slot = if *self == ItimerType::Virtual {
                    &itimers.virt
                } else {
                    &itimers.prof
                };

                if cpu_timer_slot.is_active {
                    old_itv.it_value = PosixTimeval::from_ns(cpu_timer_slot.value);
                    old_itv.it_interval = PosixTimeval::from_ns(cpu_timer_slot.interval);
                }
                old_itv
            }
        }
    }

    /// 根据定时器类型，使用新的配置来更新内部状态。
    fn set_new_value(
        &self,
        pcb: Arc<ProcessControlBlock>,
        itimers: &mut ProcessItimers,
        new_config: Itimerval,
    ) {
        match self {
            ItimerType::Real => {
                // 先取消旧的真实时间定时器
                if let Some(old_itimer) = itimers.real.take() {
                    old_itimer.timer.cancel();
                }
                // 如果 it_value 非零，创建并激活新的真实时间定时器
                if new_config.it_value.tv_sec > 0 || new_config.it_value.tv_usec > 0 {
                    let value_duration = Duration::new(
                        new_config.it_value.tv_sec as u64,
                        new_config.it_value.tv_usec as u32 * 1000,
                    );
                    let expire_jiffies =
                        timer::clock() + <Jiffies as From<Duration>>::from(value_duration).data();
                    let helper = ItimerHelper::new(pcb, ItimerType::Real, new_config.it_interval);
                    let new_timer = Timer::new(helper, expire_jiffies);
                    new_timer.activate();

                    // 将新的定时器放回 itimers
                    itimers.real = Some(crate::process::ProcessItimer {
                        timer: new_timer,
                        config: new_config,
                    });
                }
            }
            ItimerType::Virtual | ItimerType::Prof => {
                let cpu_timer_slot = if *self == ItimerType::Virtual {
                    &mut itimers.virt
                } else {
                    &mut itimers.prof
                };

                let value_ns = new_config.it_value.to_ns();
                if value_ns > 0 {
                    // 激活或重置定时器
                    cpu_timer_slot.value = value_ns;
                    cpu_timer_slot.interval = new_config.it_interval.to_ns();
                    cpu_timer_slot.is_active = true;
                } else {
                    // value 为 0，表示取消定时器
                    cpu_timer_slot.is_active = false;
                }
            }
        }
    }
}

#[derive(Debug)]
struct ItimerHelper {
    target_pcb: Weak<ProcessControlBlock>,
    which: ItimerType,
    interval: PosixTimeval,
}

impl ItimerHelper {
    fn new(
        target_pcb: Arc<ProcessControlBlock>,
        which: ItimerType,
        interval: PosixTimeval,
    ) -> Box<Self> {
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
            let _ = send_signal_to_pid(leader.raw_pid(), Signal::SIGALRM);
        } else {
            let _ = send_signal_to_pid(pcb.raw_pid(), Signal::SIGALRM);
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

fn handle_itimer(
    pcb: Arc<ProcessControlBlock>,
    which: ItimerType,
    new_value_ptr: *const Itimerval,
    old_value_ptr: *mut Itimerval,
) -> Result<usize, SystemError> {
    let mut itimers = pcb.itimers_irqsave();

    // old_value: 获取旧值并写入用户空间
    if !old_value_ptr.is_null() {
        let old_itv = which.get_current_value(&itimers);

        let mut writer = UserBufferWriter::new(old_value_ptr, size_of::<Itimerval>(), true)?;
        writer.copy_one_to_user(&old_itv, 0)?;
    }

    // new_value: 从用户空间读取新值并设置
    if !new_value_ptr.is_null() {
        let mut new_config = Itimerval::default();
        let reader = UserBufferReader::new(new_value_ptr, size_of::<Itimerval>(), true)?;
        reader.copy_one_from_user(&mut new_config, 0)?;

        which.set_new_value(pcb.clone(), &mut itimers, new_config);
    }
    Ok(0)
}

pub struct SysSetitimerHandle;

impl SysSetitimerHandle {
    fn which(args: &[usize]) -> Result<ItimerType, SystemError> {
        ItimerType::try_from(args[0] as i32)
    }

    fn new_value_ptr(args: &[usize]) -> *const Itimerval {
        args[1] as *const Itimerval
    }

    fn old_value_ptr(args: &[usize]) -> *mut Itimerval {
        args[2] as *mut Itimerval
    }
}

impl Syscall for SysSetitimerHandle {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let which = Self::which(args)?;
        let new_value_ptr = Self::new_value_ptr(args);
        let old_value_ptr = Self::old_value_ptr(args);

        let pcb = ProcessManager::current_pcb();
        handle_itimer(pcb, which, new_value_ptr, old_value_ptr)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        let which_str = match ItimerType::try_from(args[0] as i32) {
            Ok(which) => format!("{:?}", which),
            Err(_) => format!("Invalid({})", args[0]),
        };
        vec![
            FormattedSyscallParam::new("which", which_str),
            FormattedSyscallParam::new("new_value", format!("{:#x}", args[1])),
            FormattedSyscallParam::new("old_value", format!("{:#x}", args[2])),
        ]
    }
}

// 注册系统调用
syscall_table_macros::declare_syscall!(SYS_SETITIMER, SysSetitimerHandle);
