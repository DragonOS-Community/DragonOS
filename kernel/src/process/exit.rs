use alloc::sync::{Arc, Weak};
use core::intrinsics::likely;
use system_error::SystemError;

use crate::{
    arch::ipc::signal::{SigChildCode, Signal},
    driver::tty::tty_core::TtyCore,
    ipc::syscall::sys_kill::PidConverter,
    process::pid::PidType,
    sched::{SchedMode, schedule},
    syscall::user_access::UserBufferWriter,
    time::{Duration, sleep::nanosleep},
};

use super::{
    ProcessControlBlock, ProcessManager, ProcessState, RawPid, abi::WaitOption, resource::RUsage,
};

/// 内核wait4时的参数
#[derive(Debug)]
pub struct KernelWaitOption<'a> {
    pub pid_converter: PidConverter,
    pub options: WaitOption,
    pub ret_status: i32,
    pub ret_info: Option<WaitIdInfo>,
    pub ret_rusage: Option<&'a mut RUsage>,
    pub no_task_error: Option<SystemError>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct WaitIdInfo {
    pub pid: RawPid,
    pub status: i32,
    pub cause: i32,
}

impl KernelWaitOption<'_> {
    pub fn new(pid_converter: PidConverter, options: WaitOption) -> Self {
        Self {
            pid_converter,
            options,
            ret_status: 0,
            ret_info: None,
            ret_rusage: None,
            no_task_error: None,
        }
    }
}

pub fn kernel_wait4(
    pid: i32,
    wstatus_buf: Option<UserBufferWriter<'_>>,
    options: WaitOption,
    rusage_buf: Option<&mut RUsage>,
) -> Result<usize, SystemError> {
    let converter = PidConverter::from_id(pid).ok_or(SystemError::ESRCH)?;

    // 构造参数
    let mut kwo = KernelWaitOption::new(converter, options);

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

/// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/kernel/exit.c#1573
fn do_wait(kwo: &mut KernelWaitOption) -> Result<usize, SystemError> {
    let mut retval: Result<usize, SystemError>;
    let mut tmp_child_pcb: Option<Arc<ProcessControlBlock>> = None;
    macro_rules! notask {
        ($outer: lifetime) => {
            if let Some(err) = &kwo.no_task_error {
                retval = Err(err.clone());
            } else {
                retval = Ok(0);
            }

            if retval.is_err() && !kwo.options.contains(WaitOption::WNOHANG) {
                retval = Err(SystemError::ERESTARTSYS);
                if !ProcessManager::current_pcb().has_pending_signal_fast() {
                    schedule(SchedMode::SM_PREEMPT);
                    // todo: 增加子进程退出的回调后，这里可以直接等待在自身的child_wait等待队列上。
                    continue;
                } else {
                    break $outer;
                }
            } else {
                break $outer;
            }
        };
    }
    // todo: 在signal struct里面增加等待队列，并在这里初始化子进程退出的回调，使得子进程退出时，能唤醒当前进程。

    'outer: loop {
        kwo.no_task_error = Some(SystemError::ECHILD);
        match &kwo.pid_converter {
            PidConverter::Pid(pid) => {
                let child_pcb = pid
                    .pid_task(PidType::PID)
                    .ok_or(SystemError::ECHILD)
                    .unwrap();
                // 获取weak引用，以便于在do_waitpid中能正常drop pcb
                let child_weak = Arc::downgrade(&child_pcb);
                let r: Option<Result<usize, SystemError>> = do_waitpid(child_pcb, kwo);
                if let Some(r) = r {
                    retval = r;
                    break 'outer;
                } else if let Err(SystemError::ESRCH) =
                    child_weak.upgrade().unwrap().wait_queue.sleep()
                {
                    // log::debug!("do_wait: child_pcb sleep failed");
                    continue;
                }
            }
            PidConverter::All => {
                // 等待任意子进程
                // todo: 这里有问题！应当让当前进程sleep到自身的child_wait等待队列上，这样才高效。（还没实现）
                let current_pcb = ProcessManager::current_pcb();
                loop {
                    let rd_childen = current_pcb.children.read();
                    if rd_childen.is_empty() {
                        break;
                    }
                    for pid in rd_childen.iter() {
                        let pcb =
                            ProcessManager::find_task_by_vpid(*pid).ok_or(SystemError::ECHILD)?;
                        let sched_guard = pcb.sched_info().inner_lock_read_irqsave();
                        let state = sched_guard.state();
                        if state.is_exited() {
                            kwo.ret_status = state.exit_code().unwrap() as i32;
                            kwo.no_task_error = None;
                            // 由于pcb的drop方法里面要获取父进程的children字段的写锁，所以这里不能直接drop pcb，
                            // 而是要先break到外层循环，以便释放父进程的children字段的锁,才能drop pcb。
                            // 否则会死锁。
                            tmp_child_pcb = Some(pcb.clone());
                            unsafe { ProcessManager::release(pcb.raw_pid()) };
                            retval = Ok((*pid).into());
                            break 'outer;
                        }
                    }
                    drop(rd_childen);
                    nanosleep(Duration::from_millis(100).into())?;
                }
            }
            PidConverter::Pgid(Some(pgid)) => {
                loop {
                    for pcb in pgid.tasks_iter(PidType::PGID) {
                        let sched_guard = pcb.sched_info().inner_lock_read_irqsave();
                        let state = sched_guard.state();
                        if state.is_exited() {
                            kwo.ret_status = state.exit_code().unwrap() as i32;
                            kwo.no_task_error = None;
                            // 由于pcb的drop方法里面要获取父进程的children字段的写锁，所以这里不能直接drop pcb，
                            // 而是要先break到外层循环，以便释放父进程的children字段的锁,才能drop pcb。
                            // 否则会死锁。
                            tmp_child_pcb = Some(pcb.clone());
                            unsafe { ProcessManager::release(pcb.raw_pid()) };
                            retval = Ok(pcb.task_pid_vnr().into());
                            break 'outer;
                        }
                    }
                    nanosleep(Duration::from_millis(100).into())?;
                }
            }

            _ => {}
        }
        notask!('outer);
    }

    drop(tmp_child_pcb);
    ProcessManager::current_pcb()
        .sched_info
        .inner_lock_write_irqsave()
        .set_state(ProcessState::Runnable);

    // log::debug!(
    //     "do_wait, kwo.pid: {}, retval = {:?}, kwo: {:?}",
    //     kwo.pid,
    //     retval,
    //     kwo.no_task_error
    // );

    return retval;
}

fn do_waitpid(
    child_pcb: Arc<ProcessControlBlock>,
    kwo: &mut KernelWaitOption,
) -> Option<Result<usize, SystemError>> {
    let state = child_pcb.sched_info().inner_lock_read_irqsave().state();
    // 获取退出码
    match state {
        ProcessState::Runnable => {
            if kwo.options.contains(WaitOption::WNOHANG)
                || kwo.options.contains(WaitOption::WNOWAIT)
            {
                if let Some(info) = &mut kwo.ret_info {
                    *info = WaitIdInfo {
                        pid: child_pcb.raw_pid(),
                        status: Signal::SIGCONT as i32,
                        cause: SigChildCode::Continued.into(),
                    };
                } else {
                    kwo.ret_status = 0xffff;
                }

                return Some(Ok(0));
            }
        }
        ProcessState::Blocked(_) => {
            // 对于被阻塞的子进程（如正在sleep），waitpid应该继续等待
            // 而不是立即返回0。只有当子进程真正退出时才应该返回。
            return None;
        }
        ProcessState::Stopped => {
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
                    pid: child_pcb.raw_pid(),
                    status: exitcode,
                    cause: SigChildCode::Stopped.into(),
                };
            }

            return Some(Ok(child_pcb.raw_pid().data()));
        }
        ProcessState::Exited(status) => {
            let pid = child_pcb.task_pid_vnr();
            // log::debug!(
            //     "wait4: current: {}, child exited, pid: {:?}, status: {status}, \n kwo.opt: {:?}",
            //     ProcessManager::current_pid().data(),
            //     child_pcb.raw_pid(),
            //     kwo.options
            // );

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

            // debug!("wait4: to release {pid:?}");
            unsafe { ProcessManager::release(child_pcb.raw_pid()) };
            drop(child_pcb);
            return Some(Ok(pid.into()));
        }
    };

    return None;
}

impl ProcessControlBlock {
    /// 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/exit.c#143
    pub(super) fn __exit_signal(&mut self) {
        let group_dead = self.is_thread_group_leader();
        let mut sig_guard = self.sig_info_mut();
        let mut tty: Option<Arc<TtyCore>> = None;
        // log::debug!(
        //     "Process {} is exiting, group_dead: {}, state: {:?}",
        //     self.raw_pid(),
        //     group_dead,
        //     self.sched_info().inner_lock_read_irqsave().state()
        // );
        if group_dead {
            tty = sig_guard.tty();
            sig_guard.set_tty(None);
        } else {
            // todo: 通知那些等待当前线程组退出的进程
        }
        self.__unhash_process(group_dead);

        drop(tty);
    }

    /// 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/exit.c#123
    fn __unhash_process(&self, group_dead: bool) {
        self.detach_pid(PidType::PID);
        if group_dead {
            self.detach_pid(PidType::TGID);
            self.detach_pid(PidType::PGID);
            self.detach_pid(PidType::SID);
        }

        // 从线程组中移除
        let thread_group_leader = self.threads_read_irqsave().group_leader();
        if let Some(leader) = thread_group_leader {
            leader
                .threads_write_irqsave()
                .group_tasks
                .retain(|pcb| !Weak::ptr_eq(pcb, &self.self_ref));
        }
    }
}
