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
    pub struct CloneFlags: u32 {
        /// 在进程间共享文件系统信息
        const CLONE_FS = (1 << 0);
        /// 克隆时，与父进程共享信号结构体
        const CLONE_SIGNAL = (1 << 1);
        /// 克隆时，与父进程共享信号处理结构体
        const CLONE_SIGHAND = (1 << 2);
        /// 克隆时，将原本被设置为SIG_IGNORE的信号，设置回SIG_DEFAULT
        const CLONE_CLEAR_SIGHAND = (1 << 3);
        /// 在进程间共享虚拟内存空间
        const CLONE_VM = (1 << 4);
        /// 拷贝线程
        const CLONE_THREAD = (1 << 5);
        /// 共享打开的文件
        const CLONE_FILES = (1 << 6);
    }
}

impl ProcessManager {
    pub fn fork(
        current_trapframe: &mut TrapFrame,
        clone_flags: CloneFlags,
    ) -> Result<Pid, SystemError> {
        let current_pcb = ProcessManager::current_pcb();
        let new_kstack = KernelStack::new()?;
        let name = current_pcb.basic().name().to_string();
        let pcb = ProcessControlBlock::new(name, new_kstack);

        // 为内核线程设置worker private字段。（也许由内核线程机制去做会更好？）
        if current_pcb.flags().contains(ProcessFlags::KTHREAD) {
            *pcb.worker_private() = Some(WorkerPrivate::KernelThread(KernelThreadPcbPrivate::new()))
        }

        // todo: 维护父子进程关系

        // 拷贝标志位
        ProcessManager::copy_flags(&clone_flags, &pcb).unwrap_or_else(|e| {
            panic!(
                "fork: Failed to copy flags from current process, current pid: [{:?}], new pid: [{:?}]. Error: {:?}",
                current_pcb.basic().pid(), pcb.basic().pid(), e
            )
        });

        // 拷贝用户地址空间
        ProcessManager::copy_mm(&clone_flags, &current_pcb, &pcb).unwrap_or_else(|e| {
            panic!(
                "fork: Failed to copy mm from current process, current pid: [{:?}], new pid: [{:?}]. Error: {:?}",
                current_pcb.basic().pid(), pcb.basic().pid(), e
            )
        });

        // 拷贝文件描述符表
        ProcessManager::copy_files(&clone_flags, &current_pcb, &pcb).unwrap_or_else(|e| {
            panic!(
                "fork: Failed to copy files from current process, current pid: [{:?}], new pid: [{:?}]. Error: {:?}",
                current_pcb.basic().pid(), pcb.basic().pid(), e
            )
        });

        // todo: 拷贝信号相关数据

        // 拷贝线程
        ProcessManager::copy_thread(&clone_flags, &current_pcb, &pcb, &current_trapframe).unwrap_or_else(|e| {
            panic!(
                "fork: Failed to copy thread from current process, current pid: [{:?}], new pid: [{:?}]. Error: {:?}",
                current_pcb.basic().pid(), pcb.basic().pid(), e
            )
        });

        // 向procfs注册进程
        procfs_register_pid(pcb.basic().pid()).unwrap_or_else(|e| {
            panic!(
                "fork: Failed to register pid to procfs, pid: [{:?}]. Error: {:?}",
                pcb.basic().pid(),
                e
            )
        });

        ProcessManager::wakeup(&pcb).unwrap_or_else(|e| {
            panic!(
                "fork: Failed to wakeup new process, pid: [{:?}]. Error: {:?}",
                pcb.basic().pid(),
                e
            )
        });

        return Ok(pcb.basic().pid());
    }

    fn copy_flags(
        clone_flags: &CloneFlags,
        new_pcb: &Arc<ProcessControlBlock>,
    ) -> Result<(), SystemError> {
        if clone_flags.contains(CloneFlags::CLONE_VM) {
            new_pcb.flags().insert(ProcessFlags::VFORK);
        }
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
                current_pcb.basic().pid()
            )
        });

        if clone_flags.contains(CloneFlags::CLONE_VM) {
            unsafe { new_pcb.basic_mut().set_user_vm(Some(old_address_space)) };
            return Ok(());
        }

        let new_address_space = old_address_space.write().try_clone().unwrap_or_else(|e| {
            panic!(
                "copy_mm: Failed to clone address space of current process, current pid: [{:?}], new pid: [{:?}]. Error: {:?}",
                current_pcb.basic().pid(), new_pcb.basic().pid(), e
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
        }

        // 如果共享文件描述符表，则直接拷贝指针
        new_pcb
            .basic_mut()
            .set_fd_table(current_pcb.basic().fd_table().clone());

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
}
