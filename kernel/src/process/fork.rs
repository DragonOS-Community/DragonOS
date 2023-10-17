use alloc::{string::ToString, sync::Arc};

use crate::{
    arch::interrupt::TrapFrame, filesystem::procfs::procfs_register_pid, libs::rwlock::RwLock,
    process::ProcessFlags, syscall::SystemError,
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
        /// 设置其父进程中的子进程线程 ID
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
        current_trapframe: &mut TrapFrame,
        clone_flags: CloneFlags,
    ) -> Result<Pid, SystemError> {
        let current_pcb = ProcessManager::current_pcb();
        let new_kstack = KernelStack::new()?;
        let name = current_pcb.basic().name().to_string();
        let pcb = ProcessControlBlock::new(name, new_kstack);

        Self::copy_process(&clone_flags, &current_pcb, &pcb, None, current_trapframe)?;

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
        *new_pcb.flags.lock() = ProcessManager::current_pcb().flags().clone();
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

        let new_address_space = old_address_space.write().try_clone().unwrap_or_else(|e| {
            panic!(
                "copy_mm: Failed to clone address space of current process, current pid: [{:?}], new pid: [{:?}]. Error: {:?}",
                current_pcb.pid(), new_pcb.pid(), e
            )
        });
        unsafe { new_pcb.basic_mut().set_user_vm(Some(new_address_space)) };
        return Ok(());
    }

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
        _clone_flags: &CloneFlags,
        _current_pcb: &Arc<ProcessControlBlock>,
        _new_pcb: &Arc<ProcessControlBlock>,
    ) -> Result<(), SystemError> {
        // todo: 由于信号原来写的太烂，移植到新的进程管理的话，需要改动很多。因此决定重写。这里先空着
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
    /// - des_pcb 目标pcb
    /// - src_pcb 拷贝源pcb
    ///
    /// ## return
    /// - 发生错误时返回Err(SystemError)
    pub fn copy_process(
        clone_flags: &CloneFlags,
        current_pcb: &Arc<ProcessControlBlock>,
        pcb: &Arc<ProcessControlBlock>,
        usp: Option<usize>,
        current_trapframe: &mut TrapFrame,
    ) -> Result<(), SystemError> {
        // 不允许与不同命名空间的进程共享根目录
        if (*clone_flags == (CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_FS))
            || *clone_flags == (CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_FS)
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

        // 如果新进程使用不同的 pid 或用户名空间，
        // 则不允许它与分叉任务共享线程组。
        if clone_flags.contains(CloneFlags::CLONE_THREAD) {
            if clone_flags.contains(CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_NEWPID) {
                return Err(SystemError::EINVAL);
            }
            // TODO: 判断新进程与当前进程命名空间是否相同，不同则返回错误
        }

        // 如果新进程将处于不同的时间命名空间，
        // 则不能让它共享vm或线程组。
        if clone_flags.contains(CloneFlags::CLONE_THREAD | CloneFlags::CLONE_VM) {
            // TODO: 判断时间命名空间，不同则返回错误
        }

        if clone_flags.contains(CloneFlags::CLONE_PIDFD)
            && clone_flags.contains(CloneFlags::CLONE_DETACHED | CloneFlags::CLONE_THREAD)
        {
            return Err(SystemError::EINVAL);
        }

        // TODO: 克隆前应该锁信号处理，等待克隆完成后再处理

        // 克隆架构相关
        *pcb.arch_info() = current_pcb.arch_info_irqsave().clone();

        // 为内核线程设置WorkerPrivate
        if current_pcb.flags().contains(ProcessFlags::KTHREAD) {
            *pcb.worker_private() =
                Some(WorkerPrivate::KernelThread(KernelThreadPcbPrivate::new()));
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

        // todo: 拷贝信号相关数据

        // 拷贝线程
        Self::copy_thread(&clone_flags, &current_pcb, &pcb, usp ,&current_trapframe).unwrap_or_else(|e| {
            panic!(
                "fork: Failed to copy thread from current process, current pid: [{:?}], new pid: [{:?}]. Error: {:?}",
                current_pcb.pid(), pcb.pid(), e
            )
        });

        Ok(())
    }
}
