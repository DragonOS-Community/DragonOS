use core::ffi::c_void;

use alloc::{
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use system_error::SystemError;

use super::{
    abi::WaitOption,
    exit::kernel_wait4,
    fork::{CloneFlags, KernelCloneArgs},
    resource::{RLimit64, RLimitID, RUsage, RUsageWho},
    KernelStack, Pid, ProcessManager,
};
use crate::{
    arch::{interrupt::TrapFrame, MMArch},
    filesystem::{
        procfs::procfs_register_pid,
        vfs::{file::FileDescriptorVec, MAX_PATHLEN},
    },
    mm::{ucontext::UserStack, verify_area, MemoryManagementArch, VirtAddr},
    process::ProcessControlBlock,
    sched::completion::Completion,
    syscall::{
        user_access::{check_and_clone_cstr, check_and_clone_cstr_array, UserBufferWriter},
        Syscall,
    },
};

impl Syscall {
    pub fn fork(frame: &TrapFrame) -> Result<usize, SystemError> {
        ProcessManager::fork(frame, CloneFlags::empty()).map(|pid| pid.into())
    }

    pub fn vfork(frame: &TrapFrame) -> Result<usize, SystemError> {
        // 由于Linux vfork需要保证子进程先运行（除非子进程调用execve或者exit），
        // 而我们目前没有实现这个特性，所以暂时使用fork代替vfork（linux文档表示这样也是也可以的）
        Self::fork(frame)

        // 下面是以前的实现，除非我们实现了子进程先运行的特性，否则不要使用，不然会导致父进程数据损坏
        // ProcessManager::fork(
        //     frame,
        //     CloneFlags::CLONE_VM | CloneFlags::CLONE_FS | CloneFlags::CLONE_SIGNAL,
        // )
        // .map(|pid| pid.into())
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
        let options = WaitOption::from_bits(options as u32).ok_or(SystemError::EINVAL)?;

        let wstatus_buf = if wstatus.is_null() {
            None
        } else {
            Some(UserBufferWriter::new(
                wstatus,
                core::mem::size_of::<i32>(),
                true,
            )?)
        };

        let mut tmp_rusage = if rusage.is_null() {
            None
        } else {
            Some(RUsage::default())
        };

        let r = kernel_wait4(pid, wstatus_buf, options, tmp_rusage.as_mut())?;

        if !rusage.is_null() {
            let mut rusage_buf = UserBufferWriter::new::<RUsage>(
                rusage as *mut RUsage,
                core::mem::size_of::<RUsage>(),
                true,
            )?;
            rusage_buf.copy_one_to_user(&tmp_rusage.unwrap(), 0)?;
        }
        return Ok(r);
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
        current_trapframe: &TrapFrame,
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
            pcb.thread.write_irqsave().vfork_done = Some(vfork.clone());
        }

        if pcb.thread.read_irqsave().set_child_tid.is_some() {
            let addr = pcb.thread.read_irqsave().set_child_tid.unwrap();
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
        verify_area(VirtAddr::new(ptr), core::mem::size_of::<i32>())
            .map_err(|_| SystemError::EFAULT)?;

        let pcb = ProcessManager::current_pcb();
        pcb.thread.write_irqsave().clear_child_tid = Some(VirtAddr::new(ptr));
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

    /// # 设置资源限制
    ///
    /// TODO: 目前暂时不支持设置资源限制，只提供读取默认值的功能
    ///
    /// ## 参数
    ///
    /// - pid: 进程号
    /// - resource: 资源类型
    /// - new_limit: 新的资源限制
    /// - old_limit: 旧的资源限制
    ///
    /// ## 返回值
    ///
    /// - 成功，0
    /// - 如果old_limit不为NULL，则返回旧的资源限制到old_limit
    ///
    pub fn prlimit64(
        _pid: Pid,
        resource: usize,
        _new_limit: *const RLimit64,
        old_limit: *mut RLimit64,
    ) -> Result<usize, SystemError> {
        let resource = RLimitID::try_from(resource)?;
        let mut writer = None;

        if !old_limit.is_null() {
            writer = Some(UserBufferWriter::new(
                old_limit,
                core::mem::size_of::<RLimit64>(),
                true,
            )?);
        }

        match resource {
            RLimitID::Stack => {
                if let Some(mut writer) = writer {
                    let mut rlimit = writer.buffer::<RLimit64>(0).unwrap()[0];
                    rlimit.rlim_cur = UserStack::DEFAULT_USER_STACK_SIZE as u64;
                    rlimit.rlim_max = UserStack::DEFAULT_USER_STACK_SIZE as u64;
                }
                return Ok(0);
            }

            RLimitID::Nofile => {
                if let Some(mut writer) = writer {
                    let mut rlimit = writer.buffer::<RLimit64>(0).unwrap()[0];
                    rlimit.rlim_cur = FileDescriptorVec::PROCESS_MAX_FD as u64;
                    rlimit.rlim_max = FileDescriptorVec::PROCESS_MAX_FD as u64;
                }
                return Ok(0);
            }

            RLimitID::As | RLimitID::Rss => {
                if let Some(mut writer) = writer {
                    let mut rlimit = writer.buffer::<RLimit64>(0).unwrap()[0];
                    rlimit.rlim_cur = MMArch::USER_END_VADDR.data() as u64;
                    rlimit.rlim_max = MMArch::USER_END_VADDR.data() as u64;
                }
                return Ok(0);
            }

            _ => {
                return Err(SystemError::ENOSYS);
            }
        }
    }
}
