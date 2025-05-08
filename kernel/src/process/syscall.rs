use core::ffi::c_void;

use alloc::{
    ffi::CString,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use log::error;
use system_error::SystemError;

use super::{
    abi::WaitOption,
    cred::{Kgid, Kuid},
    exec::{load_binary_file, ExecParam, ExecParamFlags},
    exit::kernel_wait4,
    fork::{CloneFlags, KernelCloneArgs},
    resource::{RLimit64, RLimitID, RUsage, RUsageWho},
    KernelStack, Pgid, Pid, ProcessManager,
};
use crate::{
    arch::{interrupt::TrapFrame, CurrentIrqArch, MMArch},
    exception::InterruptArch,
    filesystem::{
        procfs::procfs_register_pid,
        vfs::{file::FileDescriptorVec, MAX_PATHLEN},
    },
    mm::{
        ucontext::{AddressSpace, UserStack},
        verify_area, MemoryManagementArch, VirtAddr,
    },
    process::ProcessControlBlock,
    sched::completion::Completion,
    syscall::{
        user_access::{check_and_clone_cstr, check_and_clone_cstr_array, UserBufferWriter},
        Syscall,
    },
};

//参考资料：https://code.dragonos.org.cn/xref/linux-6.1.9/include/uapi/linux/utsname.h#17
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PosixOldUtsName {
    pub sysname: [u8; 65],
    pub nodename: [u8; 65],
    pub release: [u8; 65],
    pub version: [u8; 65],
    pub machine: [u8; 65],
}

impl PosixOldUtsName {
    pub fn new() -> Self {
        const SYS_NAME: &[u8] = b"DragonOS";
        const NODENAME: &[u8] = b"DragonOS";
        const RELEASE: &[u8] = env!("CARGO_PKG_VERSION").as_bytes();
        const VERSION: &[u8] = env!("CARGO_PKG_VERSION").as_bytes();

        #[cfg(target_arch = "x86_64")]
        const MACHINE: &[u8] = b"x86_64";

        #[cfg(target_arch = "aarch64")]
        const MACHINE: &[u8] = b"aarch64";

        #[cfg(target_arch = "riscv64")]
        const MACHINE: &[u8] = b"riscv64";

        #[cfg(target_arch = "loongarch64")]
        const MACHINE: &[u8] = b"longarch64";

        let mut r = Self {
            sysname: [0; 65],
            nodename: [0; 65],
            release: [0; 65],
            version: [0; 65],
            machine: [0; 65],
        };

        r.sysname[0..SYS_NAME.len()].copy_from_slice(SYS_NAME);
        r.nodename[0..NODENAME.len()].copy_from_slice(NODENAME);
        r.release[0..RELEASE.len()].copy_from_slice(RELEASE);
        r.version[0..VERSION.len()].copy_from_slice(VERSION);
        r.machine[0..MACHINE.len()].copy_from_slice(MACHINE);

        return r;
    }
}

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
        // debug!(
        //     "execve path: {:?}, argv: {:?}, envp: {:?}\n",
        //     path,
        //     argv,
        //     envp
        // );
        // debug!(
        //     "before execve: strong count: {}",
        //     Arc::strong_count(&ProcessManager::current_pcb())
        // );

        if path.is_null() {
            return Err(SystemError::EINVAL);
        }

        let x = || {
            let path: CString = check_and_clone_cstr(path, Some(MAX_PATHLEN))?;
            let argv: Vec<CString> = check_and_clone_cstr_array(argv)?;
            let envp: Vec<CString> = check_and_clone_cstr_array(envp)?;
            Ok((path, argv, envp))
        };
        let (path, argv, envp) = x().inspect_err(|e: &SystemError| {
            error!("Failed to execve: {:?}", e);
        })?;

        let path = path.into_string().map_err(|_| SystemError::EINVAL)?;
        ProcessManager::current_pcb()
            .basic_mut()
            .set_name(ProcessControlBlock::generate_name(&path, &argv));

        Self::do_execve(path.clone(), argv, envp, frame)?;

        let pcb = ProcessManager::current_pcb();
        // 关闭设置了O_CLOEXEC的文件描述符
        let fd_table = pcb.fd_table();
        fd_table.write().close_on_exec();
        // debug!(
        //     "after execve: strong count: {}",
        //     Arc::strong_count(&ProcessManager::current_pcb())
        // );
        pcb.set_execute_path(path);

        return Ok(());
    }

    pub fn do_execve(
        path: String,
        argv: Vec<CString>,
        envp: Vec<CString>,
        regs: &mut TrapFrame,
    ) -> Result<(), SystemError> {
        let address_space = AddressSpace::new(true).expect("Failed to create new address space");
        // debug!("to load binary file");
        let mut param = ExecParam::new(path.as_str(), address_space.clone(), ExecParamFlags::EXEC)?;
        let old_vm = do_execve_switch_user_vm(address_space.clone());

        // 加载可执行文件
        let load_result = load_binary_file(&mut param).inspect_err(|_| {
            if let Some(old_vm) = old_vm {
                do_execve_switch_user_vm(old_vm);
            }
        })?;

        // debug!("load binary file done");
        // debug!("argv: {:?}, envp: {:?}", argv, envp);
        param.init_info_mut().args = argv;
        param.init_info_mut().envs = envp;

        // 把proc_init_info写到用户栈上
        let mut ustack_message = unsafe {
            address_space
                .write()
                .user_stack_mut()
                .expect("No user stack found")
                .clone_info_only()
        };
        let (user_sp, argv_ptr) = unsafe {
            param
                .init_info()
                .push_at(
                    // address_space
                    //     .write()
                    //     .user_stack_mut()
                    //     .expect("No user stack found"),
                    &mut ustack_message,
                )
                .expect("Failed to push proc_init_info to user stack")
        };
        address_space.write().user_stack = Some(ustack_message);

        Self::arch_do_execve(regs, &param, &load_result, user_sp, argv_ptr)
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
        // if let Some(pid_ns) = &current_pcb.get_nsproxy().read().pid_namespace {
        //     // 获取该进程在命名空间中的 PID
        //     return Ok(current_pcb.pid_strcut().read().numbers[pid_ns.level].nr);
        //     // 返回命名空间中的 PID
        // }
        // 默认返回 tgid
        Ok(current_pcb.tgid())
    }

    /// @brief 获取指定进程的pgid
    ///
    /// @param pid 指定一个进程号
    ///
    /// @return 成功，指定进程的进程组id
    /// @return 错误，不存在该进程
    pub fn getpgid(pid: Pid) -> Result<Pgid, SystemError> {
        if pid == Pid(0) {
            let current_pcb = ProcessManager::current_pcb();
            return Ok(current_pcb.pgid());
        }
        let target_proc = ProcessManager::find(pid).ok_or(SystemError::ESRCH)?;
        return Ok(target_proc.pgid());
    }

    /// 设置指定进程的pgid
    ///
    /// ## 参数
    ///
    /// - pid: 指定进程号
    /// - pgid: 新的进程组号
    ///
    /// ## 返回值
    /// 无
    pub fn setpgid(pid: Pid, pgid: Pgid) -> Result<usize, SystemError> {
        let current_pcb = ProcessManager::current_pcb();
        let pid = if pid == Pid(0) {
            current_pcb.pid()
        } else {
            pid
        };
        let pgid = if pgid == Pgid::from(0) {
            Pgid::from(pid.into())
        } else {
            pgid
        };
        if pid != current_pcb.pid() && !current_pcb.contain_child(&pid) {
            return Err(SystemError::ESRCH);
        }

        if pgid.into() != pid.into() && ProcessManager::find_process_group(pgid).is_none() {
            return Err(SystemError::EPERM);
        }
        let pcb = ProcessManager::find(pid).ok_or(SystemError::ESRCH)?;
        pcb.join_other_group(pgid)?;

        return Ok(0);
    }

    /// 创建新的会话
    pub fn setsid() -> Result<usize, SystemError> {
        let pcb = ProcessManager::current_pcb();
        let session = pcb.go_to_new_session()?;
        let mut guard = pcb.sig_info_mut();
        guard.set_tty(None);
        Ok(session.sid().into())
    }

    /// 获取指定进程的会话id
    ///
    /// 若pid为0，则返回当前进程的会话id
    ///
    /// 若pid不为0，则返回指定进程的会话id
    pub fn getsid(pid: Pid) -> Result<usize, SystemError> {
        let session = ProcessManager::current_pcb().session().unwrap();
        let sid = session.sid().into();
        if pid == Pid(0) {
            return Ok(sid);
        }
        let pcb = ProcessManager::find(pid).ok_or(SystemError::ESRCH)?;
        if !Arc::ptr_eq(&session, &pcb.session().unwrap()) {
            return Err(SystemError::EPERM);
        }
        return Ok(sid);
    }

    /// @brief 获取当前进程的父进程id
    ///
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
        let pcb = ProcessManager::current_pcb();
        return Ok(pcb.cred.lock().uid.data());
    }

    pub fn getgid() -> Result<usize, SystemError> {
        let pcb = ProcessManager::current_pcb();
        return Ok(pcb.cred.lock().gid.data());
    }

    pub fn geteuid() -> Result<usize, SystemError> {
        let pcb = ProcessManager::current_pcb();
        return Ok(pcb.cred.lock().euid.data());
    }

    pub fn getegid() -> Result<usize, SystemError> {
        let pcb = ProcessManager::current_pcb();
        return Ok(pcb.cred.lock().egid.data());
    }

    pub fn setuid(uid: usize) -> Result<usize, SystemError> {
        let pcb = ProcessManager::current_pcb();
        let mut guard = pcb.cred.lock();

        if guard.uid.data() == 0 {
            guard.setuid(uid);
            guard.seteuid(uid);
            guard.setsuid(uid);
        } else if uid == guard.uid.data() || uid == guard.suid.data() {
            guard.seteuid(uid);
        } else {
            return Err(SystemError::EPERM);
        }

        return Ok(0);
    }

    pub fn setgid(gid: usize) -> Result<usize, SystemError> {
        let pcb = ProcessManager::current_pcb();
        let mut guard = pcb.cred.lock();

        if guard.egid.data() == 0 {
            guard.setgid(gid);
            guard.setegid(gid);
            guard.setsgid(gid);
            guard.setfsgid(gid);
        } else if guard.gid.data() == gid || guard.sgid.data() == gid {
            guard.setegid(gid);
            guard.setfsgid(gid);
        } else {
            return Err(SystemError::EPERM);
        }

        return Ok(0);
    }

    pub fn seteuid(euid: usize) -> Result<usize, SystemError> {
        let pcb = ProcessManager::current_pcb();
        let mut guard = pcb.cred.lock();

        if euid == usize::MAX || (euid == guard.euid.data() && euid == guard.fsuid.data()) {
            return Ok(0);
        }

        if euid != usize::MAX {
            guard.seteuid(euid);
        }

        let euid = guard.euid.data();
        guard.setfsuid(euid);

        return Ok(0);
    }

    pub fn setegid(egid: usize) -> Result<usize, SystemError> {
        let pcb = ProcessManager::current_pcb();
        let mut guard = pcb.cred.lock();

        if egid == usize::MAX || (egid == guard.egid.data() && egid == guard.fsgid.data()) {
            return Ok(0);
        }

        if egid != usize::MAX {
            guard.setegid(egid);
        }

        let egid = guard.egid.data();
        guard.setfsgid(egid);

        return Ok(0);
    }

    pub fn setfsuid(fsuid: usize) -> Result<usize, SystemError> {
        let fsuid = Kuid::new(fsuid);

        let pcb = ProcessManager::current_pcb();
        let mut guard = pcb.cred.lock();
        let old_fsuid = guard.fsuid;

        if fsuid == guard.uid || fsuid == guard.euid || fsuid == guard.suid {
            guard.setfsuid(fsuid.data());
        }

        Ok(old_fsuid.data())
    }

    pub fn setfsgid(fsgid: usize) -> Result<usize, SystemError> {
        let fsgid = Kgid::new(fsgid);

        let pcb = ProcessManager::current_pcb();
        let mut guard = pcb.cred.lock();
        let old_fsgid = guard.fsgid;

        if fsgid == guard.gid || fsgid == guard.egid || fsgid == guard.sgid {
            guard.setfsgid(fsgid.data());
        }

        Ok(old_fsgid.data())
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

    pub fn uname(name: *mut PosixOldUtsName) -> Result<usize, SystemError> {
        let mut writer =
            UserBufferWriter::new(name, core::mem::size_of::<PosixOldUtsName>(), true)?;
        writer.copy_one_to_user(&PosixOldUtsName::new(), 0)?;

        return Ok(0);
    }
}

/// 切换用户虚拟内存空间
///
/// 该函数用于在执行系统调用 `execve` 时切换用户进程的虚拟内存空间。
///
/// # 参数
/// - `new_vm`: 新的用户地址空间，类型为 `Arc<AddressSpace>`。
///
/// # 返回值
/// - 返回旧的用户地址空间的引用，类型为 `Option<Arc<AddressSpace>>`。
///
/// # 错误处理
/// 如果地址空间切换失败，函数会触发断言失败，并输出错误信息。
fn do_execve_switch_user_vm(new_vm: Arc<AddressSpace>) -> Option<Arc<AddressSpace>> {
    // 关中断，防止在设置地址空间的时候，发生中断，然后进调度器，出现错误。
    let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
    let pcb = ProcessManager::current_pcb();
    // log::debug!(
    //     "pid: {:?}  do_execve: path: {:?}, argv: {:?}, envp: {:?}\n",
    //     pcb.pid(),
    //     path,
    //     argv,
    //     envp
    // );

    let mut basic_info = pcb.basic_mut();
    // 暂存原本的用户地址空间的引用(因为如果在切换页表之前释放了它，可能会造成内存use after free)
    let old_address_space = basic_info.user_vm();

    // 在pcb中原来的用户地址空间
    unsafe {
        basic_info.set_user_vm(None);
    }
    // 创建新的地址空间并设置为当前地址空间
    unsafe {
        basic_info.set_user_vm(Some(new_vm.clone()));
    }

    // to avoid deadlock
    drop(basic_info);

    assert!(
        AddressSpace::is_current(&new_vm),
        "Failed to set address space"
    );
    // debug!("Switch to new address space");

    // 切换到新的用户地址空间
    unsafe { new_vm.read().user_mapper.utable.make_current() };

    drop(irq_guard);

    old_address_space
}
