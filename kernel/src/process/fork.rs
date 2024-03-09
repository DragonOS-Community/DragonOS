use core::{intrinsics::unlikely, sync::atomic::Ordering};

use alloc::{string::ToString, sync::Arc};
use system_error::SystemError;

use crate::{
    arch::{interrupt::TrapFrame, ipc::signal::Signal},
    filesystem::procfs::procfs_register_pid,
    ipc::signal::flush_signal_handlers,
    libs::rwlock::RwLock,
    mm::VirtAddr,
    process::ProcessFlags,
    syscall::user_access::UserBufferWriter,
};

use super::{
    kthread::{KernelThreadPcbPrivate, WorkerPrivate},
    KernelStack, Pid, ProcessControlBlock, ProcessManager,
};

bitflags! {
    /// 进程克隆标志
    pub struct CloneFlags: u64 {
        /// 在进程间共享虚拟内存空间
        const CLONE_VM = 0x00000100;
        /// 在进程间共享文件系统信息
        const CLONE_FS = 0x00000200;
        /// 共享打开的文件
        const CLONE_FILES = 0x00000400;
        /// 克隆时，与父进程共享信号处理结构体
        const CLONE_SIGHAND = 0x00000800;
        /// 返回进程的文件描述符
        const CLONE_PIDFD = 0x00001000;
        /// 使克隆对象成为父进程的跟踪对象
        const CLONE_PTRACE = 0x00002000;
        /// 在执行 exec() 或 _exit() 之前挂起父进程的执行
        const CLONE_VFORK = 0x00004000;
        /// 使克隆对象的父进程为调用进程的父进程
        const CLONE_PARENT = 0x00008000;
        /// 拷贝线程
        const CLONE_THREAD = 0x00010000;
        /// 创建一个新的命名空间，其中包含独立的文件系统挂载点层次结构。
        const CLONE_NEWNS =	0x00020000;
        /// 与父进程共享 System V 信号量。
        const CLONE_SYSVSEM = 0x00040000;
        /// 设置其线程本地存储
        const CLONE_SETTLS = 0x00080000;
        /// 设置partent_tid地址为子进程线程 ID
        const CLONE_PARENT_SETTID = 0x00100000;
        /// 在子进程中设置一个清除线程 ID 的用户空间地址
        const CLONE_CHILD_CLEARTID = 0x00200000;
        /// 创建一个新线程，将其设置为分离状态
        const CLONE_DETACHED = 0x00400000;
        /// 使其在创建者进程或线程视角下成为无法跟踪的。
        const CLONE_UNTRACED = 0x00800000;
        /// 设置其子进程线程 ID
        const CLONE_CHILD_SETTID = 0x01000000;
        /// 将其放置在一个新的 cgroup 命名空间中
        const CLONE_NEWCGROUP = 0x02000000;
        /// 将其放置在一个新的 UTS 命名空间中
        const CLONE_NEWUTS = 0x04000000;
        /// 将其放置在一个新的 IPC 命名空间中
        const CLONE_NEWIPC = 0x08000000;
        /// 将其放置在一个新的用户命名空间中
        const CLONE_NEWUSER = 0x10000000;
        /// 将其放置在一个新的 PID 命名空间中
        const CLONE_NEWPID = 0x20000000;
        /// 将其放置在一个新的网络命名空间中
        const CLONE_NEWNET = 0x40000000;
        /// 在新的 I/O 上下文中运行它
        const CLONE_IO = 0x80000000;
        /// 克隆时，与父进程共享信号结构体
        const CLONE_SIGNAL = 0x00010000 | 0x00000800;
        /// 克隆时，将原本被设置为SIG_IGNORE的信号，设置回SIG_DEFAULT
        const CLONE_CLEAR_SIGHAND = 0x100000000;
    }
}

/// ## clone与clone3系统调用的参数载体
///
/// 因为这两个系统调用的参数很多，所以有这样一个载体更灵活
///
/// 仅仅作为参数传递
#[derive(Debug, Clone, Copy)]
pub struct KernelCloneArgs {
    pub flags: CloneFlags,

    // 下列属性均来自用户空间
    pub pidfd: VirtAddr,
    pub child_tid: VirtAddr,
    pub parent_tid: VirtAddr,
    pub set_tid: VirtAddr,

    /// 进程退出时发送的信号
    pub exit_signal: Signal,

    pub stack: usize,
    // clone3用到
    pub stack_size: usize,
    pub tls: usize,

    pub set_tid_size: usize,
    pub cgroup: i32,

    pub io_thread: bool,
    pub kthread: bool,
    pub idle: bool,
    pub func: VirtAddr,
    pub fn_arg: VirtAddr,
    // cgrp 和 cset?
}

impl KernelCloneArgs {
    pub fn new() -> Self {
        let null_addr = VirtAddr::new(0);
        Self {
            flags: unsafe { CloneFlags::from_bits_unchecked(0) },
            pidfd: null_addr,
            child_tid: null_addr,
            parent_tid: null_addr,
            set_tid: null_addr,
            exit_signal: Signal::SIGCHLD,
            stack: 0,
            stack_size: 0,
            tls: 0,
            set_tid_size: 0,
            cgroup: 0,
            io_thread: false,
            kthread: false,
            idle: false,
            func: null_addr,
            fn_arg: null_addr,
        }
    }
}

impl ProcessManager {
    /// 创建一个新进程
    ///
    /// ## 参数
    ///
    /// - `current_trapframe`: 当前进程的trapframe
    /// - `clone_flags`: 进程克隆标志
    ///
    /// ## 返回值
    ///
    /// - 成功：返回新进程的pid
    /// - 失败：返回Err(SystemError)，fork失败的话，子线程不会执行。
    ///
    /// ## Safety
    ///
    /// - fork失败的话，子线程不会执行。
    pub fn fork(
        current_trapframe: &TrapFrame,
        clone_flags: CloneFlags,
    ) -> Result<Pid, SystemError> {
        let current_pcb = ProcessManager::current_pcb();

        let new_kstack: KernelStack = KernelStack::new()?;

        let name = current_pcb.basic().name().to_string();

        let pcb = ProcessControlBlock::new(name, new_kstack);

        let mut args = KernelCloneArgs::new();
        args.flags = clone_flags;
        args.exit_signal = Signal::SIGCHLD;
        Self::copy_process(&current_pcb, &pcb, args, current_trapframe).map_err(|e| {
            kerror!(
                "fork: Failed to copy process, current pid: [{:?}], new pid: [{:?}]. Error: {:?}",
                current_pcb.pid(),
                pcb.pid(),
                e
            );
            e
        })?;
        ProcessManager::add_pcb(pcb.clone());

        // 向procfs注册进程
        procfs_register_pid(pcb.pid()).unwrap_or_else(|e| {
            panic!(
                "fork: Failed to register pid to procfs, pid: [{:?}]. Error: {:?}",
                pcb.pid(),
                e
            )
        });

        ProcessManager::wakeup(&pcb).unwrap_or_else(|e| {
            panic!(
                "fork: Failed to wakeup new process, pid: [{:?}]. Error: {:?}",
                pcb.pid(),
                e
            )
        });

        return Ok(pcb.pid());
    }

    fn copy_flags(
        clone_flags: &CloneFlags,
        new_pcb: &Arc<ProcessControlBlock>,
    ) -> Result<(), SystemError> {
        if clone_flags.contains(CloneFlags::CLONE_VM) {
            new_pcb.flags().insert(ProcessFlags::VFORK);
        }
        *new_pcb.flags.get_mut() = ProcessManager::current_pcb().flags().clone();
        return Ok(());
    }

    /// 拷贝进程的地址空间
    ///
    /// ## 参数
    ///
    /// - `clone_vm`: 是否与父进程共享地址空间。true表示共享
    /// - `new_pcb`: 新进程的pcb
    ///
    /// ## 返回值
    ///
    /// - 成功：返回Ok(())
    /// - 失败：返回Err(SystemError)
    ///
    /// ## Panic
    ///
    /// - 如果当前进程没有用户地址空间，则panic
    #[inline(never)]
    fn copy_mm(
        clone_flags: &CloneFlags,
        current_pcb: &Arc<ProcessControlBlock>,
        new_pcb: &Arc<ProcessControlBlock>,
    ) -> Result<(), SystemError> {
        let old_address_space = current_pcb.basic().user_vm().unwrap_or_else(|| {
            panic!(
                "copy_mm: Failed to get address space of current process, current pid: [{:?}]",
                current_pcb.pid()
            )
        });

        if clone_flags.contains(CloneFlags::CLONE_VM) {
            unsafe { new_pcb.basic_mut().set_user_vm(Some(old_address_space)) };
            return Ok(());
        }
        let new_address_space = old_address_space.write_irqsave().try_clone().unwrap_or_else(|e| {
            panic!(
                "copy_mm: Failed to clone address space of current process, current pid: [{:?}], new pid: [{:?}]. Error: {:?}",
                current_pcb.pid(), new_pcb.pid(), e
            )
        });
        unsafe { new_pcb.basic_mut().set_user_vm(Some(new_address_space)) };
        return Ok(());
    }

    #[inline(never)]
    fn copy_files(
        clone_flags: &CloneFlags,
        current_pcb: &Arc<ProcessControlBlock>,
        new_pcb: &Arc<ProcessControlBlock>,
    ) -> Result<(), SystemError> {
        // 如果不共享文件描述符表，则拷贝文件描述符表
        if !clone_flags.contains(CloneFlags::CLONE_FILES) {
            let new_fd_table = current_pcb.basic().fd_table().unwrap().read().clone();
            let new_fd_table = Arc::new(RwLock::new(new_fd_table));
            new_pcb.basic_mut().set_fd_table(Some(new_fd_table));
        } else {
            // 如果共享文件描述符表，则直接拷贝指针
            new_pcb
                .basic_mut()
                .set_fd_table(current_pcb.basic().fd_table().clone());
        }

        return Ok(());
    }

    #[allow(dead_code)]
    fn copy_sighand(
        clone_flags: &CloneFlags,
        current_pcb: &Arc<ProcessControlBlock>,
        new_pcb: &Arc<ProcessControlBlock>,
    ) -> Result<(), SystemError> {
        // // 将信号的处理函数设置为default(除了那些被手动屏蔽的)
        if clone_flags.contains(CloneFlags::CLONE_CLEAR_SIGHAND) {
            flush_signal_handlers(new_pcb.clone(), false);
        }

        if clone_flags.contains(CloneFlags::CLONE_SIGHAND) {
            (*new_pcb.sig_struct_irqsave()).handlers =
                current_pcb.sig_struct_irqsave().handlers.clone();
        }
        return Ok(());
    }

    /// 拷贝进程信息
    ///
    /// ## panic:
    /// 某一步拷贝失败时会引发panic
    /// 例如：copy_mm等失败时会触发panic
    ///
    /// ## 参数
    ///
    /// - clone_flags 标志位
    /// - current_pcb 拷贝源pcb
    /// - pcb 目标pcb
    ///
    /// ## return
    /// - 发生错误时返回Err(SystemError)
    #[inline(never)]
    pub fn copy_process(
        current_pcb: &Arc<ProcessControlBlock>,
        pcb: &Arc<ProcessControlBlock>,
        clone_args: KernelCloneArgs,
        current_trapframe: &TrapFrame,
    ) -> Result<(), SystemError> {
        let clone_flags = clone_args.flags;
        // 不允许与不同namespace的进程共享根目录
        if (clone_flags == (CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_FS))
            || clone_flags == (CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_FS)
        {
            return Err(SystemError::EINVAL);
        }

        // 线程组必须共享信号，分离线程只能在线程组内启动。
        if clone_flags.contains(CloneFlags::CLONE_THREAD)
            && !clone_flags.contains(CloneFlags::CLONE_SIGHAND)
        {
            return Err(SystemError::EINVAL);
        }

        // 共享信号处理器意味着共享vm。
        // 线程组也意味着共享vm。阻止这种情况可以简化其他代码。
        if clone_flags.contains(CloneFlags::CLONE_SIGHAND)
            && !clone_flags.contains(CloneFlags::CLONE_VM)
        {
            return Err(SystemError::EINVAL);
        }

        // TODO: 处理CLONE_PARENT 与 SIGNAL_UNKILLABLE的情况

        // 如果新进程使用不同的 pid 或 namespace，
        // 则不允许它与分叉任务共享线程组。
        if clone_flags.contains(CloneFlags::CLONE_THREAD) {
            if clone_flags.contains(CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_NEWPID) {
                return Err(SystemError::EINVAL);
            }
            // TODO: 判断新进程与当前进程namespace是否相同，不同则返回错误
        }

        // 如果新进程将处于不同的time namespace，
        // 则不能让它共享vm或线程组。
        if clone_flags.contains(CloneFlags::CLONE_THREAD | CloneFlags::CLONE_VM) {
            // TODO: 判断time namespace，不同则返回错误
        }

        if clone_flags.contains(CloneFlags::CLONE_PIDFD)
            && clone_flags.contains(CloneFlags::CLONE_DETACHED | CloneFlags::CLONE_THREAD)
        {
            return Err(SystemError::EINVAL);
        }

        // TODO: 克隆前应该锁信号处理，等待克隆完成后再处理

        // 克隆架构相关
        let guard = current_pcb.arch_info_irqsave();
        unsafe { pcb.arch_info().clone_from(&guard) };
        drop(guard);

        // 为内核线程设置WorkerPrivate
        if current_pcb.flags().contains(ProcessFlags::KTHREAD) {
            *pcb.worker_private() =
                Some(WorkerPrivate::KernelThread(KernelThreadPcbPrivate::new()));
        }

        // 设置clear_child_tid，在线程结束时将其置0以通知父进程
        if clone_flags.contains(CloneFlags::CLONE_CHILD_CLEARTID) {
            pcb.thread.write_irqsave().clear_child_tid = Some(clone_args.child_tid);
        }

        // 设置child_tid，意味着子线程能够知道自己的id
        if clone_flags.contains(CloneFlags::CLONE_CHILD_SETTID) {
            pcb.thread.write_irqsave().set_child_tid = Some(clone_args.child_tid);
        }

        // 将子进程/线程的id存储在用户态传进的地址中
        if clone_flags.contains(CloneFlags::CLONE_PARENT_SETTID) {
            let mut writer = UserBufferWriter::new(
                clone_args.parent_tid.data() as *mut i32,
                core::mem::size_of::<i32>(),
                true,
            )?;

            writer.copy_one_to_user(&(pcb.pid().0 as i32), 0)?;
        }

        // 拷贝标志位
        Self::copy_flags(&clone_flags, &pcb).unwrap_or_else(|e| {
            panic!(
                "fork: Failed to copy flags from current process, current pid: [{:?}], new pid: [{:?}]. Error: {:?}",
                current_pcb.pid(), pcb.pid(), e
            )
        });

        // 拷贝用户地址空间
        Self::copy_mm(&clone_flags, &current_pcb, &pcb).unwrap_or_else(|e| {
            panic!(
                "fork: Failed to copy mm from current process, current pid: [{:?}], new pid: [{:?}]. Error: {:?}",
                current_pcb.pid(), pcb.pid(), e
            )
        });

        // 拷贝文件描述符表
        Self::copy_files(&clone_flags, &current_pcb, &pcb).unwrap_or_else(|e| {
            panic!(
                "fork: Failed to copy files from current process, current pid: [{:?}], new pid: [{:?}]. Error: {:?}",
                current_pcb.pid(), pcb.pid(), e
            )
        });

        // 拷贝信号相关数据
        Self::copy_sighand(&clone_flags, &current_pcb, &pcb).map_err(|e| {
            panic!(
                "fork: Failed to copy sighand from current process, current pid: [{:?}], new pid: [{:?}]. Error: {:?}",
                current_pcb.pid(), pcb.pid(), e
            )
        })?;

        // 拷贝线程
        Self::copy_thread(&current_pcb, &pcb, clone_args,&current_trapframe).unwrap_or_else(|e| {
            panic!(
                "fork: Failed to copy thread from current process, current pid: [{:?}], new pid: [{:?}]. Error: {:?}",
                current_pcb.pid(), pcb.pid(), e
            )
        });

        // 设置线程组id、组长
        if clone_flags.contains(CloneFlags::CLONE_THREAD) {
            pcb.thread.write_irqsave().group_leader =
                current_pcb.thread.read_irqsave().group_leader.clone();
            unsafe {
                let ptr = pcb.as_ref() as *const ProcessControlBlock as *mut ProcessControlBlock;
                (*ptr).tgid = current_pcb.tgid;
            }
        } else {
            pcb.thread.write_irqsave().group_leader = Arc::downgrade(&pcb);
            unsafe {
                let ptr = pcb.as_ref() as *const ProcessControlBlock as *mut ProcessControlBlock;
                (*ptr).tgid = pcb.tgid;
            }
        }

        // CLONE_PARENT re-uses the old parent
        if clone_flags.contains(CloneFlags::CLONE_PARENT | CloneFlags::CLONE_THREAD) {
            *pcb.real_parent_pcb.write_irqsave() =
                current_pcb.real_parent_pcb.read_irqsave().clone();

            if clone_flags.contains(CloneFlags::CLONE_THREAD) {
                pcb.exit_signal.store(Signal::INVALID, Ordering::SeqCst);
            } else {
                let leader = current_pcb.thread.read_irqsave().group_leader();
                if unlikely(leader.is_none()) {
                    panic!(
                        "fork: Failed to get leader of current process, current pid: [{:?}]",
                        current_pcb.pid()
                    );
                }

                pcb.exit_signal.store(
                    leader.unwrap().exit_signal.load(Ordering::SeqCst),
                    Ordering::SeqCst,
                );
            }
        } else {
            // 新创建的进程，设置其父进程为当前进程
            *pcb.real_parent_pcb.write_irqsave() = Arc::downgrade(&current_pcb);
            pcb.exit_signal
                .store(clone_args.exit_signal, Ordering::SeqCst);
        }

        // todo: 增加线程组相关的逻辑。 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/kernel/fork.c#2437

        Ok(())
    }
}
