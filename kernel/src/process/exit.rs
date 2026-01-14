use alloc::sync::{Arc, Weak};
use core::{intrinsics::likely, sync::atomic::Ordering};
use system_error::SystemError;

use crate::{
    arch::ipc::signal::{SigChildCode, Signal},
    driver::tty::tty_core::TtyCore,
    ipc::signal_types::SignalFlags,
    ipc::syscall::sys_kill::PidConverter,
    process::pid::PidType,
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

/// 检查子进程是否可以被当前线程等待
///
/// 根据 Linux wait 语义：
/// - 默认情况下，线程组中的任何线程都可以等待同一线程组中任何线程 fork 的子进程
/// - 如果指定了 __WNOTHREAD，则只能等待当前线程自己创建的子进程
///
/// # 参数
/// - `child_pcb`: 要检查的子进程
/// - `options`: 等待选项
///
/// # 返回值
/// 返回 true 如果当前线程可以等待该子进程
fn is_eligible_child(child_pcb: &Arc<ProcessControlBlock>, options: WaitOption) -> bool {
    let current = ProcessManager::current_pcb();
    let current_tgid = current.tgid;

    // 获取子进程的 real_parent
    let child_parent = match child_pcb.real_parent_pcb() {
        Some(p) => p,
        None => return false,
    };

    if options.contains(WaitOption::WNOTHREAD) {
        // 带 __WNOTHREAD：只能等待当前线程自己创建的子进程
        // 检查子进程的 real_parent 是否就是当前线程
        Arc::ptr_eq(&child_parent, &current)
    } else {
        // 默认情况：线程组中的任何线程都可以等待同一线程组中任何线程创建的子进程
        // 检查子进程的 real_parent 的 tgid 是否与当前线程的 tgid 相同
        child_parent.tgid == current_tgid
    }
}

/// 获取当前线程组 leader 的 PCB
///
/// 用于在 wait 时遍历整个线程组的 children
fn get_thread_group_leader(pcb: &Arc<ProcessControlBlock>) -> Arc<ProcessControlBlock> {
    let ti = pcb.thread.read_irqsave();
    ti.group_leader().unwrap_or_else(|| pcb.clone())
}

/// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/kernel/exit.c#1573
fn do_wait(kwo: &mut KernelWaitOption) -> Result<usize, SystemError> {
    let mut tmp_child_pcb: Option<Arc<ProcessControlBlock>> = None;
    // todo: 在signal struct里面增加等待队列，并在这里初始化子进程退出的回调，使得子进程退出时，能唤醒当前进程。

    kwo.no_task_error = Some(SystemError::ECHILD);
    let retval = match &kwo.pid_converter {
        PidConverter::Pid(pid) => {
            if pid.pid_vnr().data() == ProcessManager::current_pcb().raw_tgid().data() {
                return Err(SystemError::ECHILD);
            }

            let child_pcb = pid.pid_task(PidType::PID).ok_or(SystemError::ECHILD)?;

            let current = ProcessManager::current_pcb();

            // 检查子进程是否可以被当前线程等待
            // 根据 Linux 语义：
            // - 默认情况下，线程组中的任何线程都可以等待同一线程组中任何线程 fork 的子进程
            // - 如果指定了 __WNOTHREAD，则只能等待当前线程自己创建的子进程
            if !is_eligible_child(&child_pcb, kwo.options) {
                return Err(SystemError::ECHILD);
            }

            // 检查子进程是否匹配等待选项（__WALL/__WCLONE）
            if !child_matches_wait_options(&child_pcb, kwo.options) {
                return Err(SystemError::ECHILD);
            }

            // 获取用于等待的 PCB（线程组 leader 或当前线程，取决于 WNOTHREAD）
            let parent = if kwo.options.contains(WaitOption::WNOTHREAD) {
                current.clone()
            } else {
                get_thread_group_leader(&current)
            };

            // 等待指定子进程：睡眠在父进程自己的 wait_queue 上
            // 子进程退出时会发送信号并唤醒父进程的 wait_queue
            loop {
                // Fast path: check without sleeping
                if let Some(r) = do_waitpid(child_pcb.clone(), kwo) {
                    break r;
                }
                if kwo.options.contains(WaitOption::WNOHANG) {
                    break Ok(0);
                }

                let mut ready: Option<Result<usize, SystemError>> = None;
                let wait_res = parent.wait_queue.wait_event_interruptible(
                    || {
                        if let Some(r) = do_waitpid(child_pcb.clone(), kwo) {
                            ready = Some(r);
                            true
                        } else {
                            false
                        }
                    },
                    None::<fn()>,
                );

                match wait_res {
                    Ok(()) => {
                        if let Some(r) = ready.take() {
                            break r;
                        }
                        if ProcessManager::current_pcb().has_pending_signal_fast() {
                            break Err(SystemError::ERESTARTSYS);
                        }
                        // 伪唤醒，继续等待
                        continue;
                    }
                    Err(e) => break Err(e),
                }
            }
        }
        PidConverter::All => {
            // 等待任意子进程：使用线程组 leader 的 wait_queue 和 children 列表
            // 这样线程组中的任何线程都可以等待同一线程组中任何线程 fork 的子进程
            let current = ProcessManager::current_pcb();
            let parent = if kwo.options.contains(WaitOption::WNOTHREAD) {
                // 带 __WNOTHREAD：只使用当前线程的 children
                current.clone()
            } else {
                // 默认：使用线程组 leader 的 children
                get_thread_group_leader(&current)
            };
            loop {
                if kwo.options.contains(WaitOption::WNOHANG) {
                    let rd_children = parent.children.read();
                    if rd_children.is_empty() {
                        break Err(SystemError::ECHILD);
                    } else {
                        break Ok(0);
                    }
                }

                let mut scan_result: Option<Result<usize, SystemError>> = None;
                let mut echild = false;

                let wait_res = parent.wait_queue.wait_event_interruptible(
                    || {
                        let rd_childen = parent.children.read();
                        if rd_childen.is_empty() {
                            echild = true;
                            return true;
                        }
                        let mut all_children_exited = true;
                        let mut pid_to_release: Option<RawPid> = None;

                        for pid in rd_childen.iter() {
                            let pcb = match ProcessManager::find_task_by_vpid(*pid) {
                                Some(p) => p,
                                None => continue,
                            };

                            if !is_eligible_child(&pcb, kwo.options) {
                                continue;
                            }
                            if !child_matches_wait_options(&pcb, kwo.options) {
                                continue;
                            }

                            let sched_guard = pcb.sched_info().inner_lock_read_irqsave();
                            let state = sched_guard.state();
                            if !pcb.is_zombie() {
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
                                kwo.ret_status = (stopsig << 8) | 0x7f;
                                if !kwo.options.contains(WaitOption::WNOWAIT) {
                                    pcb.sighand().flags_remove(SignalFlags::CLD_STOPPED);
                                }
                                scan_result = Some(Ok((*pid).into()));
                                drop(sched_guard);
                                break;
                            } else if kwo.options.contains(WaitOption::WCONTINUED)
                                && pcb.sighand().flags_contains(SignalFlags::CLD_CONTINUED)
                            {
                                kwo.no_task_error = None;
                                kwo.ret_info = Some(WaitIdInfo {
                                    pid: pcb.task_pid_vnr(),
                                    status: Signal::SIGCONT as i32,
                                    cause: SigChildCode::Continued.into(),
                                });
                                kwo.ret_status = 0xffff;
                                if !kwo.options.contains(WaitOption::WNOWAIT) {
                                    pcb.sighand().flags_remove(SignalFlags::CLD_CONTINUED);
                                }
                                scan_result = Some(Ok((*pid).into()));
                                drop(sched_guard);
                                break;
                            } else if pcb.is_zombie() && kwo.options.contains(WaitOption::WEXITED) {
                                let Some(code) = state.exit_code() else {
                                    drop(sched_guard);
                                    continue;
                                };
                                let raw = code as i32;
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
                                    if !pcb.try_mark_dead_from_zombie() {
                                        drop(sched_guard);
                                        continue;
                                    }
                                    pid_to_release = Some(pcb.raw_pid());
                                }
                                scan_result = Some(Ok((*pid).into()));
                                drop(sched_guard);
                                break;
                            }
                            drop(sched_guard);
                        }
                        drop(rd_childen);
                        if let Some(pid) = pid_to_release {
                            unsafe { ProcessManager::release(pid) };
                        }
                        if scan_result.is_some() {
                            return true;
                        }
                        if all_children_exited && !kwo.options.contains(WaitOption::WEXITED) {
                            echild = true;
                            return true;
                        }
                        false
                    },
                    None::<fn()>,
                );

                match wait_res {
                    Ok(()) => {
                        if let Some(r) = scan_result.take() {
                            break r;
                        }
                        if echild {
                            break Err(SystemError::ECHILD);
                        }
                        if ProcessManager::current_pcb().has_pending_signal_fast() {
                            break Err(SystemError::ERESTARTSYS);
                        }
                        continue;
                    }
                    Err(e) => break Err(e),
                }
            }
        }
        PidConverter::Pgid(Some(pgid)) => {
            // 修复：根据 Linux waitpid 语义，waitpid(-pgid, ...) 只等待调用者的
            // **子进程**中属于指定进程组的进程，而不是进程组中的所有进程。
            // 因此，这里遍历线程组 leader 的 children 列表，检查每个子进程是否属于目标进程组。
            let current = ProcessManager::current_pcb();
            let parent = if kwo.options.contains(WaitOption::WNOTHREAD) {
                current.clone()
            } else {
                get_thread_group_leader(&current)
            };
            loop {
                let mut scan_result: Option<Result<usize, SystemError>> = None;
                let mut echild = false;
                let wait_res = parent.wait_queue.wait_event_interruptible(
                    || {
                        let rd_children = parent.children.read();
                        if rd_children.is_empty() {
                            echild = true;
                            return true;
                        }

                        let mut has_matching_child = false;
                        let mut all_matching_children_exited = true;
                        let mut pid_to_release: Option<RawPid> = None;

                        for child_pid in rd_children.iter() {
                            let pcb = match ProcessManager::find_task_by_vpid(*child_pid) {
                                Some(p) => p,
                                None => continue,
                            };

                            if !is_eligible_child(&pcb, kwo.options) {
                                continue;
                            }

                            let child_pgrp = pcb.task_pgrp();
                            let in_target_pgrp = match &child_pgrp {
                                Some(cp) => Arc::ptr_eq(cp, pgid),
                                None => false,
                            };
                            if !in_target_pgrp {
                                continue;
                            }

                            has_matching_child = true;

                            if !child_matches_wait_options(&pcb, kwo.options) {
                                continue;
                            }

                            let sched_guard = pcb.sched_info().inner_lock_read_irqsave();
                            let state = sched_guard.state();

                            if !pcb.is_zombie() {
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
                                kwo.ret_status = (stopsig << 8) | 0x7f;
                                if !kwo.options.contains(WaitOption::WNOWAIT) {
                                    pcb.sighand().flags_remove(SignalFlags::CLD_STOPPED);
                                }
                                scan_result = Some(Ok(pcb.task_pid_vnr().into()));
                                drop(sched_guard);
                                break;
                            } else if kwo.options.contains(WaitOption::WCONTINUED)
                                && pcb.sighand().flags_contains(SignalFlags::CLD_CONTINUED)
                            {
                                kwo.no_task_error = None;
                                kwo.ret_info = Some(WaitIdInfo {
                                    pid: pcb.task_pid_vnr(),
                                    status: Signal::SIGCONT as i32,
                                    cause: SigChildCode::Continued.into(),
                                });
                                kwo.ret_status = 0xffff;
                                if !kwo.options.contains(WaitOption::WNOWAIT) {
                                    pcb.sighand().flags_remove(SignalFlags::CLD_CONTINUED);
                                }
                                scan_result = Some(Ok(pcb.task_pid_vnr().into()));
                                drop(sched_guard);
                                break;
                            } else if pcb.is_zombie() && kwo.options.contains(WaitOption::WEXITED) {
                                let Some(code) = state.exit_code() else {
                                    drop(sched_guard);
                                    continue;
                                };
                                let raw = code as i32;
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
                                    if !pcb.try_mark_dead_from_zombie() {
                                        drop(sched_guard);
                                        continue;
                                    }
                                    pid_to_release = Some(pcb.raw_pid());
                                }
                                scan_result = Some(Ok(pcb.task_pid_vnr().into()));
                                drop(sched_guard);
                                break;
                            }
                            drop(sched_guard);
                        }
                        drop(rd_children);
                        if let Some(pid) = pid_to_release {
                            unsafe { ProcessManager::release(pid) };
                        }
                        if scan_result.is_some() {
                            return true;
                        }
                        if !has_matching_child {
                            echild = true;
                            return true;
                        }
                        if all_matching_children_exited
                            && !kwo.options.contains(WaitOption::WEXITED)
                        {
                            echild = true;
                            return true;
                        }
                        false
                    },
                    None::<fn()>,
                );

                match wait_res {
                    Ok(()) => {
                        if let Some(r) = scan_result.take() {
                            break r;
                        }
                        if echild {
                            break Err(SystemError::ECHILD);
                        }
                        if kwo.options.contains(WaitOption::WNOHANG) {
                            break Ok(0);
                        }
                        if ProcessManager::current_pcb().has_pending_signal_fast() {
                            break Err(SystemError::ERESTARTSYS);
                        }
                        continue;
                    }
                    Err(e) => break Err(e),
                }
            }
        }

        PidConverter::Pgid(None) => {
            // 进程组不存在，直接返回 ECHILD
            // 这种情况发生在：进程组中的所有进程都已退出并被回收
            Err(SystemError::ECHILD)
        }
    };

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
            if !child_pcb.is_zombie() {
                return None;
            }
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
                if !child_pcb.try_mark_dead_from_zombie() {
                    drop(child_pcb);
                    return Some(Err(SystemError::ECHILD));
                }
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
