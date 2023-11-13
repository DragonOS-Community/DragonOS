use core::intrinsics::likely;

use alloc::sync::Arc;

use crate::{
    arch::{
        ipc::signal::{SigChildCode, Signal},
        sched::sched,
        CurrentIrqArch,
    },
    exception::InterruptArch,
    syscall::{user_access::UserBufferWriter, SystemError},
};

use super::{
    abi::WaitOption, pid::PidType, resource::RUsage, Pid, ProcessControlBlock, ProcessManager,
    ProcessState,
};

/// 内核wait4时的参数
#[derive(Debug)]
pub struct KernelWaitOption<'a> {
    pub pid_type: PidType,
    pub pid: Pid,
    pub options: WaitOption,
    pub ret_status: i32,
    pub ret_info: Option<WaitIdInfo>,
    pub ret_rusage: Option<&'a mut RUsage>,
    pub no_task_error: Option<SystemError>,
}

#[derive(Debug, Clone)]
pub struct WaitIdInfo {
    pub pid: Pid,
    pub status: i32,
    pub cause: i32,
}

impl<'a> KernelWaitOption<'a> {
    pub fn new(pid_type: PidType, pid: Pid, options: WaitOption) -> Self {
        Self {
            pid_type,
            pid,
            options,
            ret_status: 0,
            ret_info: None,
            ret_rusage: None,
            no_task_error: None,
        }
    }
}

pub fn kernel_wait4(
    mut pid: i64,
    wstatus_buf: Option<UserBufferWriter<'_>>,
    options: WaitOption,
    rusage_buf: Option<&mut RUsage>,
) -> Result<usize, SystemError> {
    // i64::MIN is not defined
    if pid == i64::MIN {
        return Err(SystemError::ESRCH);
    }

    // 判断pid类型
    let pidtype: PidType;

    if pid == -1 {
        pidtype = PidType::MAX;
    } else if pid < 0 {
        pidtype = PidType::PGID;
        kwarn!("kernel_wait4: currently not support pgid, default to wait for pid\n");
        pid = -pid;
    } else if pid == 0 {
        pidtype = PidType::PGID;
        kwarn!("kernel_wait4: currently not support pgid, default to wait for pid\n");
        pid = ProcessManager::current_pcb().pid().data() as i64;
    } else {
        pidtype = PidType::PID;
    }

    let pid = Pid(pid as usize);

    // 构造参数
    let mut kwo = KernelWaitOption::new(pidtype, pid, options);

    kwo.options.insert(WaitOption::WEXITED);
    kwo.ret_rusage = rusage_buf;

    // 调用do_wait，执行等待
    let r = do_wait(&mut kwo)?;

    // 如果有wstatus_buf，则将wstatus写入用户空间
    if let Some(mut wstatus_buf) = wstatus_buf {
        let wstatus = if let Some(ret_info) = &kwo.ret_info {
            ret_info.status
        } else {
            kwo.ret_status
        };
        wstatus_buf.copy_one_to_user(&wstatus, 0)?;
    }

    return Ok(r);
}

/// 参考 https://opengrok.ringotek.cn/xref/linux-6.1.9/kernel/exit.c#1573
fn do_wait(kwo: &mut KernelWaitOption) -> Result<usize, SystemError> {
    let mut retval: Result<usize, SystemError>;
    // todo: 在signal struct里面增加等待队列，并在这里初始化子进程退出的回调，使得子进程退出时，能唤醒当前进程。

    loop {
        kwo.no_task_error = Some(SystemError::ECHILD);
        let child_pcb = ProcessManager::find(kwo.pid).ok_or(SystemError::ECHILD);
        if kwo.pid_type != PidType::MAX && child_pcb.is_err() {
            if let Some(err) = &kwo.no_task_error {
                retval = Err(err.clone());
            } else {
                retval = Ok(0);
            }

            if !kwo.options.contains(WaitOption::WNOHANG) {
                retval = Err(SystemError::ERESTARTSYS);
                if ProcessManager::current_pcb()
                    .sig_info_irqsave()
                    .sig_pending()
                    .has_pending()
                    == false
                {
                    // todo: 增加子进程退出的回调后，这里可以直接等待在自身的child_wait等待队列上。
                    continue;
                } else {
                    break;
                }
            } else {
                break;
            }
        }

        if kwo.pid_type == PidType::PID {
            let child_pcb = child_pcb.unwrap();
            // 获取weak引用，以便于在do_waitpid中能正常drop pcb
            let child_weak = Arc::downgrade(&child_pcb);
            let r = do_waitpid(child_pcb, kwo);
            if r.is_some() {
                return r.unwrap();
            } else {
                child_weak.upgrade().unwrap().wait_queue.sleep();
            }
        } else if kwo.pid_type == PidType::MAX {
            // 等待任意子进程
            // todo: 这里有问题！如果正在for循环的过程中，子进程退出了，可能会导致父进程永远等待。
            let current_pcb = ProcessManager::current_pcb();
            let rd_childen = current_pcb.children.read();
            let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
            for pid in rd_childen.iter() {
                let pcb = ProcessManager::find(*pid).ok_or(SystemError::ECHILD)?;
                if pcb.sched_info().state().is_exited() {
                    kwo.ret_status = pcb.sched_info().state().exit_code().unwrap() as i32;
                    drop(pcb);
                    unsafe { ProcessManager::release(pid.clone()) };
                    return Ok(pid.clone().into());
                } else {
                    unsafe { pcb.wait_queue.sleep_without_schedule() };
                }
            }
            drop(irq_guard);
            sched();
        } else {
            // todo: 对于pgid的处理
            kwarn!("kernel_wait4: currently not support {:?}", kwo.pid_type);
            return Err(SystemError::EINVAL);
        }
    }

    return retval;
}

fn do_waitpid(
    child_pcb: Arc<ProcessControlBlock>,
    kwo: &mut KernelWaitOption,
) -> Option<Result<usize, SystemError>> {
    let state = child_pcb.sched_info().state();
    // 获取退出码
    match state {
        ProcessState::Runnable => {
            if kwo.options.contains(WaitOption::WNOHANG)
                || kwo.options.contains(WaitOption::WNOWAIT)
            {
                if let Some(info) = &mut kwo.ret_info {
                    *info = WaitIdInfo {
                        pid: child_pcb.pid(),
                        status: Signal::SIGCONT as i32,
                        cause: SigChildCode::Continued.into(),
                    };
                } else {
                    kwo.ret_status = 0xffff;
                }

                return Some(Ok(0));
            }
        }
        ProcessState::Blocked(_) | ProcessState::Stopped => {
            // todo: 在stopped里面，添加code字段，表示停止的原因
            let exitcode = 0;
            // 由于目前不支持ptrace，因此这个值为false
            let ptrace = false;

            if (!ptrace) && (!kwo.options.contains(WaitOption::WUNTRACED)) {
                kwo.ret_status = 0;
                return Some(Ok(0));
            }

            if likely(!(kwo.options.contains(WaitOption::WNOWAIT))) {
                kwo.ret_status = (exitcode << 8) | 0x7f;
            }
            if let Some(infop) = &mut kwo.ret_info {
                *infop = WaitIdInfo {
                    pid: child_pcb.pid(),
                    status: exitcode,
                    cause: SigChildCode::Stopped.into(),
                };
            }

            return Some(Ok(child_pcb.pid().data()));
        }
        ProcessState::Exited(status) => {
            let pid = child_pcb.pid();
            // kdebug!("wait4: child exited, pid: {:?}, status: {status}\n", pid);

            if likely(!kwo.options.contains(WaitOption::WEXITED)) {
                return None;
            }

            // todo: 增加对线程组的group leader的处理

            if let Some(infop) = &mut kwo.ret_info {
                *infop = WaitIdInfo {
                    pid,
                    status: status as i32,
                    cause: SigChildCode::Exited.into(),
                };
            }

            kwo.ret_status = status as i32;

            drop(child_pcb);
            // kdebug!("wait4: to release {pid:?}");
            unsafe { ProcessManager::release(pid) };
            return Some(Ok(pid.into()));
        }
    };

    return None;
}
