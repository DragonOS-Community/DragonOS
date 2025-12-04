use alloc::sync::{Arc, Weak};
use core::{intrinsics::likely, sync::atomic::Ordering};
use system_error::SystemError;

use crate::{
    arch::ipc::signal::{SigChildCode, Signal},
    driver::tty::tty_core::TtyCore,
    ipc::signal_types::SignalFlags,
    ipc::syscall::sys_kill::PidConverter,
    process::pid::PidType,
    sched::{schedule, SchedMode},
    syscall::user_access::UserBufferWriter,
};

use super::{
    abi::WaitOption, resource::RUsage, ProcessControlBlock, ProcessManager, ProcessState, RawPid,
};

/// 将内核中保存的 wstatus（已经按 wait4 语义左移过的编码值）
/// 转换为 waitid 语义下的 si_status（低 8 位退出码）。
#[inline(always)]
fn wstatus_to_waitid_status(raw_wstatus: i32) -> i32 {
    (raw_wstatus >> 8) & 0xff
}

/// 检查子进程的 exit_signal 是否与等待选项匹配
///
/// 根据 Linux wait 语义：
/// - __WALL: 等待所有子进程，忽略 exit_signal
/// - __WCLONE: 只等待"克隆"子进程（exit_signal != SIGCHLD）
/// - 默认（无 __WCLONE）: 只等待"正常"子进程（exit_signal == SIGCHLD）
fn child_matches_wait_options(child_pcb: &Arc<ProcessControlBlock>, options: WaitOption) -> bool {
    // __WALL 匹配所有子进程
    if options.contains(WaitOption::WALL) {
        return true;
    }

    let child_exit_signal = child_pcb.exit_signal.load(Ordering::SeqCst);
    let is_clone_child = child_exit_signal != Signal::SIGCHLD;
    let wants_clone = options.contains(WaitOption::WCLONE);

    // 子进程类型必须与等待选项匹配
    is_clone_child == wants_clone
}

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
    let converter = PidConverter::from_id(pid).ok_or(SystemError::ECHILD)?;

    // 构造参数
    let mut kwo = KernelWaitOption::new(converter, options);

    kwo.options.insert(WaitOption::WEXITED);
    kwo.ret_rusage = rusage_buf;

    // 调用do_wait，执行等待
    let r = do_wait(&mut kwo)?;

    // 如果有wstatus_buf，则将wstatus写入用户空间
    if let Some(mut wstatus_buf) = wstatus_buf {
        // wait4 路径始终返回 wstatus（编码值），不能使用 ret_info
        let wstatus = kwo.ret_status;
        wstatus_buf.copy_one_to_user(&wstatus, 0)?;
    }

    return Ok(r);
}

/// waitid 的内核实现：基于 do_wait，返回 0，必要时写回 siginfo 与 rusage
pub fn kernel_waitid(
    pid_selector: PidConverter,
    mut infop: Option<UserBufferWriter<'_>>, // PosixSigInfo
    options: WaitOption,
    rusage_buf: Option<&mut RUsage>,
) -> Result<usize, SystemError> {
    // 构造参数
    let mut kwo = KernelWaitOption::new(pid_selector, options);
    kwo.ret_rusage = rusage_buf;
    // waitid 不强制 WEXITED，由调用者通过 options 指定

    // 走通用等待
    let _ = do_wait(&mut kwo)?;

    // 写回 siginfo（若提供）
    if let Some(mut writer) = infop.take() {
        // log::debug!(
        //     "kernel_waitid: about to write PosixSigInfo, sizeof={} bytes, user_buf_size={} bytes",
        //     core::mem::size_of::<PosixSigInfo>(),
        //     writer.size()
        // );
        use crate::ipc::signal_types::{PosixSigInfo, PosixSiginfoFields, PosixSiginfoSigchld};
        let mut si = PosixSigInfo {
            si_signo: 0,
            si_errno: 0,
            si_code: 0,
            _sifields: PosixSiginfoFields {
                _kill: crate::ipc::signal_types::PosixSiginfoKill {
                    si_pid: 0,
                    si_uid: 0,
                },
            },
        };
        if let Some(info) = &kwo.ret_info {
            si.si_signo = Signal::SIGCHLD as i32; // SIGCHLD
            si.si_errno = 0;
            si.si_code = info.cause; // CLD_*
            si._sifields = PosixSiginfoFields {
                _sigchld: PosixSiginfoSigchld {
                    si_pid: info.pid.data() as i32,
                    si_uid: 0,
                    si_status: info.status,
                    si_utime: 0,
                    si_stime: 0,
                },
            };
        }
        writer.copy_one_to_user(&si, 0)?;
        // if let Some(info) = &kwo.ret_info {
        //     log::debug!(
        //         "kernel_waitid: wrote siginfo: signo={}, code={}, pid={}, status={}",
        //         si.si_signo,
        //         si.si_code,
        //         info.pid.data(),
        //         info.status
        //     );
        // } else {
        //     log::debug!(
        //         "kernel_waitid: wrote empty siginfo (no event): signo=0, code=0"
        //     );
        // }
    }

    Ok(0)
}

/// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/kernel/exit.c#1573
fn do_wait(kwo: &mut KernelWaitOption) -> Result<usize, SystemError> {
    let mut retval: Result<usize, SystemError> = Ok(0);
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
                if pid.pid_vnr().data() == ProcessManager::current_pcb().raw_tgid().data() {
                    return Err(SystemError::ECHILD);
                }

                let child_pcb = pid.pid_task(PidType::PID).ok_or(SystemError::ECHILD)?;

                let parent = ProcessManager::current_pcb();

                // 检查是否是当前进程的子进程，否则返回ECHILD
                let is_child = parent.children.read().contains(&child_pcb.raw_pid());
                if !is_child {
                    return Err(SystemError::ECHILD);
                }

                // 检查子进程是否匹配等待选项（__WALL/__WCLONE）
                if !child_matches_wait_options(&child_pcb, kwo.options) {
                    return Err(SystemError::ECHILD);
                }

                // 等待指定子进程：睡眠在父进程自己的 wait_queue 上
                // 子进程退出时会发送信号并唤醒父进程的 wait_queue
                loop {
                    // Fast path: check without sleeping
                    if let Some(r) = do_waitpid(child_pcb.clone(), kwo) {
                        retval = r;

                        break 'outer;
                    }
                    if kwo.options.contains(WaitOption::WNOHANG) {
                        retval = Ok(0);
                        break 'outer;
                    }

                    // 睡眠在父进程自己的 wait_queue 上
                    if let Err(e) = parent.wait_queue.prepare_to_wait_event(true) {
                        if e == SystemError::ERESTARTSYS {
                            retval = Err(SystemError::ERESTARTSYS);
                            break 'outer;
                        } else if e == SystemError::ECHILD {
                            // 队列已死亡，不应该发生
                            retval = Err(SystemError::ECHILD);
                            break 'outer;
                        } else {
                            retval = Err(e);
                            break 'outer;
                        }
                    }

                    // Re-check after registration to avoid lost wakeup
                    if let Some(r) = do_waitpid(child_pcb.clone(), kwo) {
                        parent.wait_queue.finish_wait();

                        retval = r;
                        break 'outer;
                    }

                    // Sleep until child state changes (will be woken by SIGCHLD or wait_queue.wakeup_all)
                    schedule(SchedMode::SM_NONE);
                    // Leave the queue before next iteration
                    parent.wait_queue.finish_wait();
                    // 继续循环，重新检查子进程状态
                }
            }
            PidConverter::All => {
                // 等待任意子进程：使用父进程的 wait_queue，避免丢唤醒
                let parent = ProcessManager::current_pcb();
                loop {
                    // 注册等待
                    let _ = parent.wait_queue.prepare_to_wait_event(true);

                    let rd_childen = parent.children.read();
                    if rd_childen.is_empty() {
                        parent.wait_queue.finish_wait();
                        break;
                    }
                    let mut found = false;
                    // 标记是否所有子进程都已退出（僵尸状态，不会再改变）
                    let mut all_children_exited = true;
                    // 保存需要 release 的 pid，避免在持有 children 锁时调用 release 导致死锁
                    let mut pid_to_release: Option<RawPid> = None;
                    for pid in rd_childen.iter() {
                        let pcb =
                            ProcessManager::find_task_by_vpid(*pid).ok_or(SystemError::ECHILD)?;

                        // 检查子进程是否匹配等待选项（__WALL/__WCLONE）
                        if !child_matches_wait_options(&pcb, kwo.options) {
                            continue;
                        }

                        let sched_guard = pcb.sched_info().inner_lock_read_irqsave();
                        let state = sched_guard.state();

                        // 如果有子进程不是 Exited 状态，则标记为 false
                        if !state.is_exited() {
                            all_children_exited = false;
                        }

                        if matches!(state, ProcessState::Stopped)
                            && kwo.options.contains(WaitOption::WSTOPPED)
                            && pcb.sighand().flags_contains(SignalFlags::CLD_STOPPED)
                        {
                            let stopsig = Signal::SIGSTOP as i32;
                            kwo.no_task_error = None;
                            kwo.ret_info = Some(WaitIdInfo {
                                pid: pcb.task_pid_vnr(),
                                status: stopsig,
                                cause: SigChildCode::Stopped.into(),
                            });
                            // Linux wait(2) 语义：stopped 进程的 wstatus = (stopsig << 8) | 0x7f
                            kwo.ret_status = (stopsig << 8) | 0x7f;
                            if !kwo.options.contains(WaitOption::WNOWAIT) {
                                pcb.sighand().flags_remove(SignalFlags::CLD_STOPPED);
                            }
                            retval = Ok((*pid).into());
                            found = true;
                            // 延迟 drop sched_guard
                        } else if kwo.options.contains(WaitOption::WCONTINUED)
                            && pcb.sighand().flags_contains(SignalFlags::CLD_CONTINUED)
                        {
                            kwo.no_task_error = None;
                            kwo.ret_info = Some(WaitIdInfo {
                                pid: pcb.task_pid_vnr(),
                                status: Signal::SIGCONT as i32,
                                cause: SigChildCode::Continued.into(),
                            });
                            // Linux wait(2) 语义：continued 进程的 wstatus = 0xffff
                            kwo.ret_status = 0xffff;
                            if !kwo.options.contains(WaitOption::WNOWAIT) {
                                pcb.sighand().flags_remove(SignalFlags::CLD_CONTINUED);
                            }
                            retval = Ok((*pid).into());
                            found = true;
                        } else if state.is_exited() && kwo.options.contains(WaitOption::WEXITED) {
                            let raw = state.exit_code().unwrap() as i32;
                            kwo.ret_status = raw;
                            let status8 = wstatus_to_waitid_status(raw);
                            kwo.no_task_error = None;
                            kwo.ret_info = Some(WaitIdInfo {
                                pid: pcb.task_pid_vnr(),
                                status: status8,
                                cause: SigChildCode::Exited.into(),
                            });
                            tmp_child_pcb = Some(pcb.clone());
                            if !kwo.options.contains(WaitOption::WNOWAIT) {
                                // 延迟 release，避免死锁
                                pid_to_release = Some(pcb.raw_pid());
                            }
                            retval = Ok((*pid).into());
                            found = true;
                        }
                        drop(sched_guard);
                        if found {
                            break;
                        }
                    }
                    drop(rd_childen);
                    // 在释放 children 锁后再调用 release
                    if let Some(pid) = pid_to_release {
                        unsafe { ProcessManager::release(pid) };
                    }
                    if found {
                        parent.wait_queue.finish_wait();
                        break 'outer;
                    }
                    if kwo.options.contains(WaitOption::WNOHANG) {
                        parent.wait_queue.finish_wait();
                        retval = Ok(0);
                        break 'outer;
                    }

                    // 关键修复：如果所有子进程都已退出（僵尸状态），且没有请求 WEXITED
                    // 那么永远不会有匹配的事件发生，应该立即返回 ECHILD 而不是继续等待
                    if all_children_exited && !kwo.options.contains(WaitOption::WEXITED) {
                        parent.wait_queue.finish_wait();
                        retval = Err(SystemError::ECHILD);
                        break 'outer; // 直接退出到外层循环，绕过 notask! 宏
                    }

                    // 无事件，睡眠
                    schedule(SchedMode::SM_NONE);
                    parent.wait_queue.finish_wait();
                }
            }
            PidConverter::Pgid(Some(pgid)) => {
                // 修复：根据 Linux waitpid 语义，waitpid(-pgid, ...) 只等待调用者的
                // **子进程**中属于指定进程组的进程，而不是进程组中的所有进程。
                // 因此，这里遍历父进程的 children 列表，检查每个子进程是否属于目标进程组。
                let parent = ProcessManager::current_pcb();
                loop {
                    // 注册等待
                    let _ = parent.wait_queue.prepare_to_wait_event(true);

                    let rd_children = parent.children.read();
                    if rd_children.is_empty() {
                        parent.wait_queue.finish_wait();
                        // 没有任何子进程
                        retval = Err(SystemError::ECHILD);
                        break 'outer;
                    }

                    let mut found = false;
                    // 是否找到了属于目标进程组的子进程（无论其状态如何）
                    let mut has_matching_child = false;
                    let mut all_matching_children_exited = true;
                    // 保存需要 release 的 pid，避免在持有 children 锁时调用 release 导致死锁
                    let mut pid_to_release: Option<RawPid> = None;

                    for child_pid in rd_children.iter() {
                        let pcb = match ProcessManager::find_task_by_vpid(*child_pid) {
                            Some(p) => p,
                            None => {
                                continue;
                            }
                        };

                        // 检查子进程是否属于目标进程组
                        // 注意：即使进程已退出并从进程组的 tasks 列表中 detach，
                        // task_pgrp() 仍然返回它之前所属的进程组
                        let child_pgrp = pcb.task_pgrp();
                        let in_target_pgrp = match &child_pgrp {
                            Some(cp) => Arc::ptr_eq(cp, pgid),
                            None => false,
                        };

                        if !in_target_pgrp {
                            continue;
                        }

                        // 找到属于目标进程组的子进程
                        has_matching_child = true;

                        // 检查子进程是否匹配等待选项（__WALL/__WCLONE）
                        if !child_matches_wait_options(&pcb, kwo.options) {
                            continue;
                        }

                        let sched_guard = pcb.sched_info().inner_lock_read_irqsave();
                        let state = sched_guard.state();

                        if !state.is_exited() {
                            all_matching_children_exited = false;
                        }

                        if matches!(state, ProcessState::Stopped)
                            && kwo.options.contains(WaitOption::WSTOPPED)
                            && pcb.sighand().flags_contains(SignalFlags::CLD_STOPPED)
                        {
                            let stopsig = Signal::SIGSTOP as i32;
                            kwo.no_task_error = None;
                            kwo.ret_info = Some(WaitIdInfo {
                                pid: pcb.task_pid_vnr(),
                                status: stopsig,
                                cause: SigChildCode::Stopped.into(),
                            });
                            // Linux wait(2) 语义：stopped 进程的 wstatus = (stopsig << 8) | 0x7f
                            kwo.ret_status = (stopsig << 8) | 0x7f;
                            if !kwo.options.contains(WaitOption::WNOWAIT) {
                                pcb.sighand().flags_remove(SignalFlags::CLD_STOPPED);
                            }
                            retval = Ok(pcb.task_pid_vnr().into());
                            found = true;
                        } else if kwo.options.contains(WaitOption::WCONTINUED)
                            && pcb.sighand().flags_contains(SignalFlags::CLD_CONTINUED)
                        {
                            kwo.no_task_error = None;
                            kwo.ret_info = Some(WaitIdInfo {
                                pid: pcb.task_pid_vnr(),
                                status: Signal::SIGCONT as i32,
                                cause: SigChildCode::Continued.into(),
                            });
                            // Linux wait(2) 语义：continued 进程的 wstatus = 0xffff
                            kwo.ret_status = 0xffff;
                            if !kwo.options.contains(WaitOption::WNOWAIT) {
                                pcb.sighand().flags_remove(SignalFlags::CLD_CONTINUED);
                            }
                            retval = Ok(pcb.task_pid_vnr().into());
                            found = true;
                        } else if state.is_exited() && kwo.options.contains(WaitOption::WEXITED) {
                            let raw = state.exit_code().unwrap() as i32;
                            kwo.ret_status = raw;
                            let status8 = wstatus_to_waitid_status(raw);
                            kwo.no_task_error = None;
                            kwo.ret_info = Some(WaitIdInfo {
                                pid: pcb.task_pid_vnr(),
                                status: status8,
                                cause: SigChildCode::Exited.into(),
                            });
                            tmp_child_pcb = Some(pcb.clone());
                            if !kwo.options.contains(WaitOption::WNOWAIT) {
                                // 延迟 release，避免死锁
                                pid_to_release = Some(pcb.raw_pid());
                            }
                            retval = Ok(pcb.task_pid_vnr().into());
                            found = true;
                        }
                        drop(sched_guard);
                        if found {
                            break;
                        }
                    }
                    drop(rd_children);
                    // 在释放 children 锁后再调用 release
                    if let Some(pid) = pid_to_release {
                        unsafe { ProcessManager::release(pid) };
                    }

                    if found {
                        parent.wait_queue.finish_wait();
                        break 'outer;
                    }

                    // 如果没有找到任何属于目标进程组的子进程，返回 ECHILD
                    if !has_matching_child {
                        parent.wait_queue.finish_wait();
                        retval = Err(SystemError::ECHILD);
                        break 'outer;
                    }

                    if kwo.options.contains(WaitOption::WNOHANG) {
                        parent.wait_queue.finish_wait();
                        retval = Ok(0);
                        break 'outer;
                    }

                    // 如果所有匹配的子进程都已退出，且没有请求 WEXITED
                    // 那么永远不会有匹配的事件发生，应该立即返回 ECHILD
                    if all_matching_children_exited && !kwo.options.contains(WaitOption::WEXITED) {
                        parent.wait_queue.finish_wait();
                        retval = Err(SystemError::ECHILD);
                        break 'outer;
                    }

                    schedule(SchedMode::SM_NONE);
                    parent.wait_queue.finish_wait();
                }
            }

            PidConverter::Pgid(None) => {
                // 进程组不存在，直接返回 ECHILD
                // 这种情况发生在：进程组中的所有进程都已退出并被回收
                return Err(SystemError::ECHILD);
            }
        }
        notask!('outer);
    }

    drop(tmp_child_pcb);

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
    // 优先处理继续事件：与 Linux 语义一致，只要标志存在即可报告
    if kwo.options.contains(WaitOption::WCONTINUED)
        && child_pcb
            .sighand()
            .flags_contains(SignalFlags::CLD_CONTINUED)
    {
        // log::debug!(
        //     "do_waitpid: report CLD_CONTINUED for pid={:?}",
        //     child_pcb.raw_pid()
        // );
        kwo.ret_info = Some(WaitIdInfo {
            pid: child_pcb.task_pid_vnr(),
            status: Signal::SIGCONT as i32,
            cause: SigChildCode::Continued.into(),
        });

        // 设置 ret_status 供 wait4 使用
        // Linux wait(2) 语义：continued 进程的 wstatus = 0xffff
        kwo.ret_status = 0xffff;

        if !kwo.options.contains(WaitOption::WNOWAIT) {
            child_pcb.sighand().flags_remove(SignalFlags::CLD_CONTINUED);
        }
        return Some(Ok(child_pcb.raw_pid().data()));
    }

    let state = child_pcb.sched_info().inner_lock_read_irqsave().state();
    // 获取退出码
    match state {
        ProcessState::Runnable => {
            if kwo.options.contains(WaitOption::WNOHANG) {
                return Some(Ok(0));
            }
        }
        ProcessState::Blocked(_) => {
            // 对于被阻塞的子进程（如正在sleep），waitpid应该继续等待
            // 而不是立即返回0。只有当子进程真正退出时才应该返回。
            return None;
        }
        ProcessState::Stopped => {
            // 非 ptrace 停止：报告 stopsig=SIGSTOP
            let stopsig = Signal::SIGSTOP as i32;
            // 由于目前不支持ptrace，因此这个值为false
            let ptrace = false;

            if (!ptrace) && (!kwo.options.contains(WaitOption::WSTOPPED)) {
                // 调用方未请求 WSTOPPED，按照 Linux 语义应当继续等待其它事件
                // 而不是返回 0 并写回空的 siginfo。
                return None;
            }

            // 填充 waitid 信息
            // log::debug!("do_waitpid: report CLD_STOPPED for pid={:?}", child_pcb.raw_pid());
            kwo.ret_info = Some(WaitIdInfo {
                pid: child_pcb.task_pid_vnr(),
                status: stopsig,
                cause: SigChildCode::Stopped.into(),
            });

            // 设置 ret_status 供 wait4 使用
            // Linux wait(2) 语义：stopped 进程的 wstatus = (stopsig << 8) | 0x7f
            kwo.ret_status = (stopsig << 8) | 0x7f;

            if !kwo.options.contains(WaitOption::WNOWAIT) {
                // 消费一次停止事件标志（若存在）
                child_pcb.sighand().flags_remove(SignalFlags::CLD_STOPPED);
            }

            return Some(Ok(child_pcb.raw_pid().data()));
        }
        ProcessState::Exited(status) => {
            let pid = child_pcb.task_pid_vnr();
            // Linux 语义：若等待集合未包含 WEXITED，则不报告退出事件
            if likely(!kwo.options.contains(WaitOption::WEXITED)) {
                return None;
            }

            // 始终填充 waitid 信息
            // log::debug!("do_waitpid: report CLD_EXITED for pid={:?}", child_pcb.raw_pid());
            kwo.ret_info = Some(WaitIdInfo {
                pid,
                status: wstatus_to_waitid_status(status as i32),
                cause: SigChildCode::Exited.into(),
            });

            kwo.ret_status = status as i32;

            // 若指定 WNOWAIT，则只观测不回收
            if !kwo.options.contains(WaitOption::WNOWAIT) {
                unsafe { ProcessManager::release(child_pcb.raw_pid()) };
                drop(child_pcb);
            } else {
                // 观测模式下不回收，保持任务可再次被 wait 系列看到
                drop(child_pcb);
            }
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
