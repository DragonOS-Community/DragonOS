use core::ffi::c_void;

use alloc::{
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};

use super::{
    abi::WaitOption,
    fork::{CloneFlags, KernelCloneArgs},
    resource::{RUsage, RUsageWho},
    KernelStack, Pid, ProcessManager, ProcessState,
};
use crate::{
    arch::{interrupt::TrapFrame, sched::sched, CurrentIrqArch},
    exception::InterruptArch,
    filesystem::{procfs::procfs_register_pid, vfs::MAX_PATHLEN},
    include::bindings::bindings::verify_area,
    mm::VirtAddr,
    process::ProcessControlBlock,
    sched::completion::Completion,
    syscall::{
        user_access::{
            check_and_clone_cstr, check_and_clone_cstr_array, UserBufferReader, UserBufferWriter,
        },
        Syscall, SystemError,
    },
};

impl Syscall {
    pub fn fork(frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let r = ProcessManager::fork(frame, CloneFlags::empty()).map(|pid| pid.into());
        return r;
    }

    pub fn vfork(frame: &mut TrapFrame) -> Result<usize, SystemError> {
        ProcessManager::fork(
            frame,
            CloneFlags::CLONE_VM | CloneFlags::CLONE_FS | CloneFlags::CLONE_SIGNAL,
        )
        .map(|pid| pid.into())
    }

    pub fn execve(
        path: *const u8,
        argv: *const *const u8,
        envp: *const *const u8,
        frame: &mut TrapFrame,
    ) -> Result<(), SystemError> {
        // kdebug!(
        //     "execve path: {:?}, argv: {:?}, envp: {:?}\n",
        //     path,
        //     argv,
        //     envp
        // );
        // kdebug!(
        //     "before execve: strong count: {}",
        //     Arc::strong_count(&ProcessManager::current_pcb())
        // );

        if path.is_null() {
            return Err(SystemError::EINVAL);
        }

        let x = || {
            let path: String = check_and_clone_cstr(path, Some(MAX_PATHLEN))?;
            let argv: Vec<String> = check_and_clone_cstr_array(argv)?;
            let envp: Vec<String> = check_and_clone_cstr_array(envp)?;
            Ok((path, argv, envp))
        };
        let r: Result<(String, Vec<String>, Vec<String>), SystemError> = x();
        if let Err(e) = r {
            panic!("Failed to execve: {:?}", e);
        }
        let (path, argv, envp) = r.unwrap();
        ProcessManager::current_pcb()
            .basic_mut()
            .set_name(ProcessControlBlock::generate_name(&path, &argv));

        Self::do_execve(path, argv, envp, frame)?;

        // 关闭设置了O_CLOEXEC的文件描述符
        let fd_table = ProcessManager::current_pcb().fd_table();
        fd_table.write().close_on_exec();
        // kdebug!(
        //     "after execve: strong count: {}",
        //     Arc::strong_count(&ProcessManager::current_pcb())
        // );

        return Ok(());
    }

    pub fn wait4(
        pid: i64,
        wstatus: *mut i32,
        options: i32,
        rusage: *mut c_void,
    ) -> Result<usize, SystemError> {
        let ret = WaitOption::from_bits(options as u32);
        let options = match ret {
            Some(options) => options,
            None => {
                return Err(SystemError::EINVAL);
            }
        };

        let mut _rusage_buf =
            UserBufferReader::new::<c_void>(rusage, core::mem::size_of::<c_void>(), true)?;

        let mut wstatus_buf =
            UserBufferWriter::new::<i32>(wstatus, core::mem::size_of::<i32>(), true)?;

        let cur_pcb = ProcessManager::current_pcb();
        let rd_childen = cur_pcb.children.read();

        if pid > 0 {
            let pid = Pid(pid as usize);
            let child_pcb = ProcessManager::find(pid).ok_or(SystemError::ECHILD)?;
            drop(rd_childen);

            loop {
                let state = child_pcb.sched_info().state();
                // 获取退出码
                match state {
                    ProcessState::Runnable => {
                        if options.contains(WaitOption::WNOHANG)
                            || options.contains(WaitOption::WNOWAIT)
                        {
                            if !wstatus.is_null() {
                                wstatus_buf.copy_one_to_user(&WaitOption::WCONTINUED.bits(), 0)?;
                            }
                            return Ok(0);
                        }
                    }
                    ProcessState::Blocked(_) | ProcessState::Stopped => {
                        // 指定WUNTRACED则等待暂停的进程，不指定则返回0
                        if !options.contains(WaitOption::WUNTRACED)
                            || options.contains(WaitOption::WNOWAIT)
                        {
                            if !wstatus.is_null() {
                                wstatus_buf.copy_one_to_user(&WaitOption::WSTOPPED.bits(), 0)?;
                            }
                            return Ok(0);
                        }
                    }
                    ProcessState::Exited(status) => {
                        // kdebug!("wait4: child exited, pid: {:?}, status: {status}\n", pid);
                        if !wstatus.is_null() {
                            wstatus_buf.copy_one_to_user(
                                &(status as u32 | WaitOption::WEXITED.bits()),
                                0,
                            )?;
                        }
                        drop(child_pcb);
                        // kdebug!("wait4: to release {pid:?}");
                        unsafe { ProcessManager::release(pid) };
                        return Ok(pid.into());
                    }
                };

                // 等待指定进程
                child_pcb.wait_queue.sleep();
            }
        } else if pid < -1 {
            // TODO 判断是否pgid == -pid（等待指定组任意进程）
            // 暂时不支持
            return Err(SystemError::EINVAL);
        } else if pid == 0 {
            // TODO 判断是否pgid == current_pgid（等待当前组任意进程）
            // 暂时不支持
            return Err(SystemError::EINVAL);
        } else {
            // 等待任意子进程(这两)
            let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
            for pid in rd_childen.iter() {
                let pcb = ProcessManager::find(*pid).ok_or(SystemError::ECHILD)?;
                if pcb.sched_info().state().is_exited() {
                    if !wstatus.is_null() {
                        wstatus_buf.copy_one_to_user(&0, 0)?;
                    }
                    return Ok(pid.clone().into());
                } else {
                    unsafe { pcb.wait_queue.sleep_without_schedule() };
                }
            }
            drop(irq_guard);
            sched();
        }

        return Ok(0);
    }

    /// # 退出进程
    ///
    /// ## 参数
    ///
    /// - status: 退出状态
    pub fn exit(status: usize) -> ! {
        ProcessManager::exit(status);
    }

    /// @brief 获取当前进程的pid
    pub fn getpid() -> Result<Pid, SystemError> {
        let current_pcb = ProcessManager::current_pcb();
        return Ok(current_pcb.tgid());
    }

    /// @brief 获取指定进程的pgid
    ///
    /// @param pid 指定一个进程号
    ///
    /// @return 成功，指定进程的进程组id
    /// @return 错误，不存在该进程
    pub fn getpgid(mut pid: Pid) -> Result<Pid, SystemError> {
        if pid == Pid(0) {
            let current_pcb = ProcessManager::current_pcb();
            pid = current_pcb.pid();
        }
        let target_proc = ProcessManager::find(pid).ok_or(SystemError::ESRCH)?;
        return Ok(target_proc.basic().pgid());
    }
    /// @brief 获取当前进程的父进程id

    /// 若为initproc则ppid设置为0   
    pub fn getppid() -> Result<Pid, SystemError> {
        let current_pcb = ProcessManager::current_pcb();
        return Ok(current_pcb.basic().ppid());
    }

    pub fn clone(
        current_trapframe: &mut TrapFrame,
        clone_args: KernelCloneArgs,
    ) -> Result<usize, SystemError> {
        let flags = clone_args.flags;

        let vfork = Arc::new(Completion::new());

        if flags.contains(CloneFlags::CLONE_PIDFD)
            && flags.contains(CloneFlags::CLONE_PARENT_SETTID)
        {
            return Err(SystemError::EINVAL);
        }

        let current_pcb = ProcessManager::current_pcb();
        let new_kstack = KernelStack::new()?;
        let name = current_pcb.basic().name().to_string();
        let pcb = ProcessControlBlock::new(name, new_kstack);
        // 克隆pcb
        ProcessManager::copy_process(&current_pcb, &pcb, clone_args, current_trapframe)?;
        ProcessManager::add_pcb(pcb.clone());

        // 向procfs注册进程
        procfs_register_pid(pcb.pid()).unwrap_or_else(|e| {
            panic!(
                "fork: Failed to register pid to procfs, pid: [{:?}]. Error: {:?}",
                pcb.pid(),
                e
            )
        });

        if flags.contains(CloneFlags::CLONE_VFORK) {
            pcb.thread.write().vfork_done = Some(vfork.clone());
        }

        if pcb.thread.read().set_child_tid.is_some() {
            let addr = pcb.thread.read().set_child_tid.unwrap();
            let mut writer =
                UserBufferWriter::new(addr.as_ptr::<i32>(), core::mem::size_of::<i32>(), true)?;
            writer.copy_one_to_user(&(pcb.pid().data() as i32), 0)?;
        }

        ProcessManager::wakeup(&pcb).unwrap_or_else(|e| {
            panic!(
                "fork: Failed to wakeup new process, pid: [{:?}]. Error: {:?}",
                pcb.pid(),
                e
            )
        });

        if flags.contains(CloneFlags::CLONE_VFORK) {
            // 等待子进程结束或者exec;
            vfork.wait_for_completion_interruptible()?;
        }

        return Ok(pcb.pid().0);
    }

    /// 设置线程地址
    pub fn set_tid_address(ptr: usize) -> Result<usize, SystemError> {
        if !unsafe { verify_area(ptr as u64, core::mem::size_of::<i32>() as u64) } {
            return Err(SystemError::EFAULT);
        }

        let pcb = ProcessManager::current_pcb();
        pcb.thread.write().clear_child_tid = Some(VirtAddr::new(ptr));
        Ok(pcb.pid.0)
    }

    pub fn gettid() -> Result<Pid, SystemError> {
        let pcb = ProcessManager::current_pcb();
        Ok(pcb.pid)
    }

    pub fn getuid() -> Result<usize, SystemError> {
        // todo: 增加credit功能之后，需要修改
        return Ok(0);
    }

    pub fn getgid() -> Result<usize, SystemError> {
        // todo: 增加credit功能之后，需要修改
        return Ok(0);
    }

    pub fn geteuid() -> Result<usize, SystemError> {
        // todo: 增加credit功能之后，需要修改
        return Ok(0);
    }

    pub fn getegid() -> Result<usize, SystemError> {
        // todo: 增加credit功能之后，需要修改
        return Ok(0);
    }

    pub fn get_rusage(who: i32, rusage: *mut RUsage) -> Result<usize, SystemError> {
        let who = RUsageWho::try_from(who)?;
        let mut writer = UserBufferWriter::new(rusage, core::mem::size_of::<RUsage>(), true)?;
        let pcb = ProcessManager::current_pcb();
        let rusage = pcb.get_rusage(who).ok_or(SystemError::EINVAL)?;

        let ubuf = writer.buffer::<RUsage>(0).unwrap();
        ubuf.copy_from_slice(&[rusage]);

        return Ok(0);
    }
}
