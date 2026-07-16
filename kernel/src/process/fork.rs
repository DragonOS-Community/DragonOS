use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use crate::arch::MMArch;
use crate::cgroup::{cgroup_accounting_lock, cgroup_can_fork_in, cgroup_migrate_vet_dst_with_src};
use crate::filesystem::cgroup2::{cgroup2_check_attach_permissions, cgroup2_inode_to_node};
use crate::filesystem::vfs::file::File;
use crate::filesystem::vfs::file::FileFlags;
use crate::filesystem::vfs::file::ReservedFd;
use crate::filesystem::vfs::FileType;
use crate::mm::access_ok;
use crate::mm::MemoryManagementArch;
use crate::process::pidfd::PidFd;
use alloc::{string::ToString, sync::Arc};
use log::{error, warn};
use system_error::SystemError;

use crate::{
    arch::{interrupt::TrapFrame, ipc::signal::Signal},
    ipc::signal_types::SignalFlags,
    libs::{cpumask::CpuMask, rwsem::RwSem},
    mm::VirtAddr,
    process::ProcessFlags,
    sched::{cpu_is_online, sched_cgroup_fork, sched_fork},
    smp::cpu::{smp_cpu_manager_initialized, ProcessorId},
    syscall::user_access::UserBufferWriter,
};

use super::{
    account_successful_fork, alloc_pid, inc_visible_thread_count,
    kthread::{KernelThreadPcbPrivate, WorkerPrivate},
    lock_fs_refs_copy,
    pid::{Pid, PidType},
    FsRefsReadGuard, KernelStack, ProcessControlBlock, ProcessManager, RawPid,
    PTRACE_RELATION_LOCK,
};
pub const MAX_PID_NS_LEVEL: usize = 32;

bitflags! {
    /// 进程克隆标志
    pub struct CloneFlags: u64 {
        const CLONE_NEWTIME = 0x00000080;
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
        /// 克隆时，将原本被设置为SIG_IGNORE的信号，设置回SIG_DEFAULT
        const CLONE_CLEAR_SIGHAND = 0x100000000;
        /// 克隆到具有正确权限的特定cgroup中
        const CLONE_INTO_CGROUP = 0x200000000;
    }
}

/// ## clone与clone3系统调用的参数载体
///
/// 因为这两个系统调用的参数很多，所以有这样一个载体更灵活
///
/// 仅仅作为参数传递
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct KernelCloneArgs {
    pub flags: CloneFlags,
    pub target_cpu: Option<ProcessorId>,
    pub cpus_allowed: Option<CpuMask>,

    // 下列属性均来自用户空间
    pub pidfd: VirtAddr,
    pub child_tid: VirtAddr,
    pub parent_tid: VirtAddr,
    pub set_tid: Vec<usize>,

    /// 进程退出时发送给父进程的信号。
    ///
    /// Linux 的 task_struct::exit_signal 使用整数语义：
    /// - -1: 非线程组 leader（CLONE_THREAD），不可被普通 wait 回收；
    /// - 0: 退出时不发送信号，但仍是可被 __WCLONE 等待的 clone 子进程；
    /// - >0: 退出时发送对应信号。
    pub exit_signal: i32,

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
            target_cpu: None,
            cpus_allowed: None,
            pidfd: null_addr,
            child_tid: null_addr,
            parent_tid: null_addr,
            set_tid: Vec::with_capacity(MAX_PID_NS_LEVEL),
            exit_signal: Signal::SIGCHLD as i32,
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

    pub fn verify(&self) -> Result<(), SystemError> {
        if self.flags.contains(CloneFlags::CLONE_SETTLS) {
            access_ok(VirtAddr::new(self.tls), MMArch::PAGE_SIZE)
                .map_err(|_| SystemError::EPERM)?;
        }
        Ok(())
    }

    #[inline]
    fn target_cpu_is_online(cpu: ProcessorId) -> bool {
        !smp_cpu_manager_initialized() || cpu_is_online(cpu)
    }

    #[inline]
    fn target_cpu_is_allowed(cpu: ProcessorId, allowed: &CpuMask) -> bool {
        allowed.get(cpu).unwrap_or(false)
    }

    #[inline]
    fn validate_target_cpu(cpu: ProcessorId, allowed: &CpuMask) -> Result<(), SystemError> {
        if Self::target_cpu_is_allowed(cpu, allowed) && Self::target_cpu_is_online(cpu) {
            Ok(())
        } else {
            Err(SystemError::EINVAL)
        }
    }

    #[inline]
    pub fn validate_requested_target_cpu(&self, allowed: &CpuMask) -> Result<(), SystemError> {
        if let Some(target_cpu) = self.target_cpu {
            Self::validate_target_cpu(target_cpu, allowed)?;
        }

        Ok(())
    }

    #[inline]
    #[allow(dead_code)]
    pub fn resolve_target_cpu(
        &mut self,
        default_cpu: ProcessorId,
        allowed: &CpuMask,
    ) -> Result<ProcessorId, SystemError> {
        let target_cpu = if let Some(target_cpu) = self.target_cpu {
            Self::validate_target_cpu(target_cpu, allowed)?;
            target_cpu
        } else if Self::target_cpu_is_allowed(default_cpu, allowed)
            && Self::target_cpu_is_online(default_cpu)
        {
            default_cpu
        } else {
            allowed
                .iter_cpu()
                .find(|&cpu| Self::target_cpu_is_online(cpu))
                .ok_or(SystemError::EINVAL)?
        };

        self.target_cpu = Some(target_cpu);
        Ok(target_cpu)
    }

    /// 规范化 exit_signal 字段，根据 Linux clone 语义处理
    ///
    /// ## 规则
    ///
    /// 1. 如果设置了 CLONE_THREAD，进程是线程组成员，不应发送 exit_signal（设为 -1）
    /// 2. 其他情况保持 exit_signal 不变
    ///
    /// 这个方法应该在 do_clone() 之前调用，确保 exit_signal 的语义正确。
    pub fn normalize_exit_signal(&mut self) {
        if self.flags.contains(CloneFlags::CLONE_THREAD) {
            // 线程组成员不是线程组 leader，Linux 中 exit_signal 为 -1。
            self.exit_signal = -1;
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
    ) -> Result<RawPid, SystemError> {
        let mut args = KernelCloneArgs::new();
        args.flags = clone_flags;
        args.exit_signal = Signal::SIGCHLD as i32;
        Self::fork_with_args(current_trapframe, args)
    }

    pub fn fork_with_args(
        current_trapframe: &TrapFrame,
        args: KernelCloneArgs,
    ) -> Result<RawPid, SystemError> {
        let current_pcb = ProcessManager::current_pcb();
        let caller_pid_ns = if current_pcb.raw_pid().data() == 0 {
            None
        } else {
            Some(current_pcb.active_pid_ns())
        };

        let new_kstack: KernelStack = KernelStack::new()?;

        let name = current_pcb.basic().name().to_string();

        args.verify()?;
        let pcb = ProcessControlBlock::new(name, new_kstack);
        Self::copy_process(&current_pcb, &pcb, args, current_trapframe).map_err(|e| {
            error!(
                "fork: Failed to copy process, current pid: [{:?}], new pid: [{:?}]. Error: {:?}",
                current_pcb.raw_pid(),
                pcb.raw_pid(),
                e
            );
            e
        })?;
        // if pcb.raw_pid().data() > 1 {
        //     log::debug!(
        //         "fork done, pid: {}, pgid: {:?}, tgid: {:?}, sid: {}",
        //         pcb.raw_pid(),
        //         pcb.task_pgrp().map(|x| x.pid_vnr().data()),
        //         pcb.task_tgid_vnr(),
        //         pcb.task_session().map_or(0, |s| s.pid_vnr().data())
        //     );
        // }

        ProcessManager::wake_up_new_task(&pcb).unwrap_or_else(|e| {
            panic!(
                "fork: Failed to wakeup new process, pid: [{:?}]. Error: {:?}",
                pcb.raw_pid(),
                e
            )
        });

        if ProcessManager::current_pid().data() == 0 {
            return Ok(pcb.raw_pid());
        }

        return pcb
            .task_pid_nr_ns(PidType::PID, caller_pid_ns)
            .ok_or(SystemError::EINVAL);
    }

    fn copy_flags(
        clone_flags: &CloneFlags,
        kthread: bool,
        new_pcb: &Arc<ProcessControlBlock>,
    ) -> Result<(), SystemError> {
        let parent_flags = *ProcessManager::current_pcb().flags();
        let mut child_flags = parent_flags.fork_inherited();

        // KTHREAD 不通过继承传递，而是由创建参数显式赋予。
        if kthread {
            child_flags.insert(ProcessFlags::KTHREAD);
        }

        if clone_flags.contains(CloneFlags::CLONE_VFORK) {
            child_flags.insert(ProcessFlags::VFORK);
        }
        child_flags.insert(ProcessFlags::FORKNOEXEC);

        *new_pcb.flags.get_mut() = child_flags;

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
    /// - 无
    #[inline(never)]
    fn copy_mm(
        clone_flags: &CloneFlags,
        current_pcb: &Arc<ProcessControlBlock>,
        new_pcb: &Arc<ProcessControlBlock>,
    ) -> Result<(), SystemError> {
        let old_address_space = current_pcb.basic().user_vm().ok_or(SystemError::ENOMEM)?;

        if clone_flags.contains(CloneFlags::CLONE_VM) {
            unsafe { new_pcb.basic_mut().set_user_vm(Some(old_address_space)) };
            return Ok(());
        }
        let new_address_space = old_address_space
            .try_clone_wait()
            .map_err(|_| SystemError::ENOMEM)?;
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
            let new_fd_table = current_pcb.basic().try_fd_table().unwrap().read().clone();
            let new_fd_table = Arc::new(RwSem::new(new_fd_table));
            new_pcb.basic_mut().set_fd_table(Some(new_fd_table));
        } else {
            // 如果共享文件描述符表，则直接拷贝指针
            new_pcb
                .basic_mut()
                .set_fd_table(current_pcb.basic().try_fd_table().clone());
        }

        return Ok(());
    }

    #[allow(dead_code)]
    fn copy_sighand(
        clone_flags: &CloneFlags,
        current_pcb: &Arc<ProcessControlBlock>,
        new_pcb: &Arc<ProcessControlBlock>,
    ) -> Result<(), SystemError> {
        if clone_flags.contains(CloneFlags::CLONE_SIGHAND) {
            new_pcb.replace_sighand(current_pcb.sighand());
            return Ok(());
        }

        // log::debug!("Just copy sighand");
        new_pcb.sighand().copy_handlers_from(&current_pcb.sighand());

        if clone_flags.contains(CloneFlags::CLONE_CLEAR_SIGHAND) {
            new_pcb.flush_signal_handlers(false);
        }
        return Ok(());
    }

    /// 复制进程信号信息（sig_info）
    ///
    /// fork 时需要复制父进程的信号掩码（sig_blocked）等信息到子进程。
    /// execve 时应该保留 sig_blocked，不重置。
    ///
    /// 参考 Linux: kernel/fork.c copy_process()
    fn copy_sig_info(
        _clone_flags: &CloneFlags,
        current_pcb: &Arc<ProcessControlBlock>,
        new_pcb: &Arc<ProcessControlBlock>,
    ) -> Result<(), SystemError> {
        // 只复制信号掩码 - POSIX 要求 fork 和 execve 都保留信号掩码
        // 注意：先读取父进程的，然后释放锁，再写入子进程的，避免死锁
        let oom_score_owner =
            ProcessManager::find(current_pcb.raw_tgid()).unwrap_or_else(|| current_pcb.clone());
        let sig_info_state = if Arc::ptr_eq(&oom_score_owner, current_pcb) {
            let sig_info = current_pcb.sig_info_irqsave();
            (
                *sig_info.sig_blocked(),
                sig_info.oom_score_adj(),
                sig_info.oom_score_adj_min(),
            )
        } else {
            let sig_blocked = {
                let current_sig_info = current_pcb.sig_info_irqsave();
                *current_sig_info.sig_blocked()
            };
            let (oom_score_adj, oom_score_adj_min) = {
                let oom_score_owner_sig_info = oom_score_owner.sig_info_irqsave();
                (
                    oom_score_owner_sig_info.oom_score_adj(),
                    oom_score_owner_sig_info.oom_score_adj_min(),
                )
            };
            (sig_blocked, oom_score_adj, oom_score_adj_min)
        };

        {
            let mut new_sig_info = new_pcb.sig_info_mut();
            *new_sig_info.sig_block_mut() = sig_info_state.0;
            new_sig_info.set_oom_score_adj(sig_info_state.1);
            new_sig_info.set_oom_score_adj_min(sig_info_state.2);
        }

        Ok(())
    }

    fn needs_oom_score_adj_clone_vm_sync(clone_flags: &CloneFlags) -> bool {
        (*clone_flags & (CloneFlags::CLONE_VM | CloneFlags::CLONE_THREAD | CloneFlags::CLONE_VFORK))
            == CloneFlags::CLONE_VM
    }

    fn sync_oom_score_adj_for_clone_vm(
        current_pcb: &Arc<ProcessControlBlock>,
        new_pcb: &Arc<ProcessControlBlock>,
    ) {
        let oom_score_owner =
            ProcessManager::find(current_pcb.raw_tgid()).unwrap_or_else(|| current_pcb.clone());
        let (oom_score_adj, oom_score_adj_min) = {
            let current_sig_info = oom_score_owner.sig_info_irqsave();
            (
                current_sig_info.oom_score_adj(),
                current_sig_info.oom_score_adj_min(),
            )
        };

        let mut new_sig_info = new_pcb.sig_info_mut();
        new_sig_info.set_oom_score_adj(oom_score_adj);
        new_sig_info.set_oom_score_adj_min(oom_score_adj_min);
    }

    /// 复制 prctl 相关的进程/线程状态。
    ///
    /// - no_new_privs：线程级语义，clone/fork 继承，execve 保持（execve 不走这里）。
    /// - keepcaps：clone/fork 继承。
    /// - dumpable：fork 继承。
    fn copy_prctl_state(
        _clone_flags: &CloneFlags,
        current_pcb: &Arc<ProcessControlBlock>,
        new_pcb: &Arc<ProcessControlBlock>,
    ) -> Result<(), SystemError> {
        // NO_NEW_PRIVS
        if current_pcb.no_new_privs() != 0 {
            new_pcb.set_no_new_privs(true);
        }

        // KEEPCAPS
        new_pcb.set_keepcaps(current_pcb.keepcaps());

        // DUMPABLE
        new_pcb.set_dumpable(current_pcb.dumpable());
        Ok(())
    }

    /// 拷贝信号备用栈
    ///
    /// ## 参数
    ///
    /// - `clone_flags`: 克隆标志
    /// - `current_pcb`: 当前进程的PCB
    /// - `new_pcb`: 新进程的PCB
    ///
    /// ## 返回值
    ///
    /// - 成功：返回Ok(())
    /// - 失败：返回Err(SystemError)
    ///
    /// ## 说明
    ///
    /// 根据Linux语义：
    /// - fork()时，子进程应该继承父进程的sigaltstack设置
    /// - clone(CLONE_THREAD)时（创建线程），新线程应该有一个清空的sigaltstack
    ///
    /// 参考：
    /// - https://man7.org/linux/man-pages/man2/sigaltstack.2.html
    fn copy_sigaltstack(
        clone_flags: &CloneFlags,
        current_pcb: &Arc<ProcessControlBlock>,
        new_pcb: &Arc<ProcessControlBlock>,
    ) -> Result<(), SystemError> {
        // 如果是创建线程（CLONE_THREAD），则不继承sigaltstack
        // 新线程应该有一个空的信号备用栈
        if clone_flags.contains(CloneFlags::CLONE_THREAD) {
            // 新线程的sig_altstack已经在new()时初始化为空，无需额外操作
            return Ok(());
        }

        // fork()时，子进程继承父进程的sigaltstack设置
        let parent_altstack = current_pcb.sig_altstack();
        let mut child_altstack = new_pcb.sig_altstack_mut();
        *child_altstack = *parent_altstack;

        Ok(())
    }

    /// 拷贝进程信息
    ///
    /// ## panic:
    /// 某一步拷贝失败时会引发panic
    /// 例如：copy_mm等失败时会触发panic
    ///
    /// ## 参数
    ///
    /// - clone_args 克隆参数。注意，在传入这里之前，clone_args应当已经通过verify()函数校验。
    /// - current_pcb 拷贝源pcb
    /// - pcb 目标pcb
    ///
    /// ## return
    /// - 成功时返回Ok(())
    /// - 发生错误时返回Err(SystemError)
    #[inline(never)]
    pub fn copy_process(
        current_pcb: &Arc<ProcessControlBlock>,
        pcb: &Arc<ProcessControlBlock>,
        mut clone_args: KernelCloneArgs,
        current_trapframe: &TrapFrame,
    ) -> Result<(), SystemError> {
        let clone_flags = clone_args.flags;
        // 不允许与不同namespace的进程共享根目录

        // exec 去线程化期间不允许创建新线程
        if clone_flags.contains(CloneFlags::CLONE_THREAD)
            && (current_pcb
                .sighand()
                .flags_contains(SignalFlags::GROUP_EXEC)
                || current_pcb
                    .sighand()
                    .flags_contains(SignalFlags::GROUP_EXIT))
        {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }

        if (clone_flags & (CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_FS)
            == (CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_FS))
            || (clone_flags & (CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_FS))
                == (CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_FS)
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

        // 当全局init进程或其容器内init进程的兄弟进程退出时，由于它们的父进程（swapper）不会回收它们，
        // 这些进程会保持僵尸状态。为了避免这种情况以及防止出现多根进程树，
        // 需要阻止全局init和容器内init进程创建兄弟进程。
        if clone_flags.contains(CloneFlags::CLONE_PARENT)
            && current_pcb
                .sighand()
                .flags_contains(SignalFlags::UNKILLABLE)
        {
            return Err(SystemError::EINVAL);
        }

        // 如果新进程使用不同的 pid 或 namespace，
        // 则不允许它与分叉任务共享线程组。
        if clone_flags.contains(CloneFlags::CLONE_THREAD)
            && (!((clone_flags & (CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_NEWPID))
                .is_empty())
                || !Arc::ptr_eq(
                    &current_pcb.active_pid_ns(),
                    current_pcb.nsproxy().pid_namespace_for_children(),
                ))
        {
            return Err(SystemError::EINVAL);
        }

        // 如果新进程将处于不同的time namespace，
        // 则不能让它共享vm或线程组。
        if !((clone_flags & (CloneFlags::CLONE_THREAD | CloneFlags::CLONE_VM)).is_empty()) {
            // TODO: 判断time namespace，不同则返回错误
        }

        if clone_flags.contains(CloneFlags::CLONE_PIDFD)
            && !((clone_flags & (CloneFlags::CLONE_DETACHED | CloneFlags::CLONE_THREAD)).is_empty())
        {
            return Err(SystemError::EINVAL);
        }

        // TODO: 克隆前应该锁信号处理，等待克隆完成后再处理

        // 克隆架构相关
        let mut guard = current_pcb.arch_info_irqsave();
        guard.sync_current_state_before_fork();
        unsafe {
            pcb.arch_info().clone_from(&guard);
        }
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

        sched_fork(pcb).unwrap_or_else(|e| {
            panic!(
                "fork: Failed to set sched info from current process, current pid: [{:?}], new pid: [{:?}]. Error: {:?}",
                current_pcb.raw_pid(), pcb.raw_pid(), e
            )
        });
        // pid 0 是 bootstrap idle 线程。它的 affinity 可能是“当前 CPU only”，
        // 但普通任务不能把这份临时/per-cpu mask 当作默认继承下来。
        if current_pcb.raw_pid() != RawPid(0) {
            pcb.sched_info()
                .set_cpus_allowed(current_pcb.sched_info().cpus_allowed());
        }
        if let Some(cpus_allowed) = clone_args.cpus_allowed.take() {
            pcb.sched_info().set_cpus_allowed(cpus_allowed);
        }
        clone_args.validate_requested_target_cpu(&pcb.sched_info().cpus_allowed())?;

        // 拷贝标志位
        Self::copy_flags(&clone_flags, clone_args.kthread, pcb).unwrap_or_else(|e| {
            panic!(
                "fork: Failed to copy flags from current process, current pid: [{:?}], new pid: [{:?}]. Error: {:?}",
                current_pcb.raw_pid(), pcb.raw_pid(), e
            )
        });

        // 拷贝 prctl 状态（no_new_privs/dumpable 等）
        Self::copy_prctl_state(&clone_flags, current_pcb, pcb).unwrap_or_else(|e| {
            panic!(
                "fork: Failed to copy prctl state from current process, current pid: [{:?}], new pid: [{:?}]. Error: {:?}",
                current_pcb.raw_pid(), pcb.raw_pid(), e
            )
        });

        crate::process::seccomp::copy_seccomp(
            current_pcb
                .seccomp_mode
                .load(core::sync::atomic::Ordering::Relaxed),
            &current_pcb.seccomp_filter,
            &pcb.seccomp_mode,
            &pcb.seccomp_filter,
        );

        // 拷贝文件描述符表
        Self::copy_files(&clone_flags, current_pcb, pcb).unwrap_or_else(|e| {
            panic!(
                "fork: Failed to copy files from current process, current pid: [{:?}], new pid: [{:?}]. Error: {:?}",
                current_pcb.raw_pid(), pcb.raw_pid(), e
            )
        });

        // Keep the fs_struct snapshot and child publication on the same side
        // of pivot_root's exclusive migration barrier.
        let fs_refs_copy = lock_fs_refs_copy();

        // 拷贝文件系统信息
        Self::copy_fs(&clone_flags, current_pcb, pcb, &fs_refs_copy).unwrap_or_else(|e| {
            panic!(
                "fork: Failed to copy fs from current process, current pid: [{:?}], new pid: [{:?}]. Error: {:?}",
                current_pcb.raw_pid(), pcb.raw_pid(), e
            )
        });

        // 拷贝信号相关数据
        Self::copy_sighand(&clone_flags, current_pcb, pcb).unwrap_or_else(|e| {
            panic!(
                "fork: Failed to copy sighand from current process, current pid: [{:?}], new pid: [{:?}]. Error: {:?}",
                current_pcb.raw_pid(), pcb.raw_pid(), e
            )
        });

        // 拷贝信号信息（sig_info，包括信号掩码）
        Self::copy_sig_info(&clone_flags, current_pcb, pcb).unwrap_or_else(|e| {
            panic!(
                "fork: Failed to copy sig_info from current process, current pid: [{:?}], new pid: [{:?}]. Error: {:?}",
                current_pcb.raw_pid(), pcb.raw_pid(), e
            )
        });

        // 拷贝信号备用栈
        Self::copy_sigaltstack(&clone_flags, current_pcb, pcb).unwrap_or_else(|e| {
            panic!(
                "fork: Failed to copy sigaltstack from current process, current pid: [{:?}], new pid: [{:?}]. Error: {:?}",
                current_pcb.raw_pid(), pcb.raw_pid(), e
            )
        });

        // 拷贝用户地址空间
        Self::copy_mm(&clone_flags, current_pcb, pcb).unwrap_or_else(|e| {
            panic!(
                "fork: Failed to copy mm from current process, current pid: [{:?}], new pid: [{:?}]. Error: {:?}",
                current_pcb.raw_pid(), pcb.raw_pid(), e
            )
        });

        // 拷贝namespace
        Self::copy_namespaces(&clone_flags, current_pcb, pcb, &fs_refs_copy).unwrap_or_else(|e| {
            panic!(
                "fork: Failed to copy namespaces from current process, current pid: [{:?}], new pid: [{:?}]. Error: {:?}",
                current_pcb.raw_pid(), pcb.raw_pid(), e
            )
        });

        // 拷贝线程
        Self::copy_thread(current_pcb, pcb, &clone_args, current_trapframe).unwrap_or_else(|e| {
            panic!(
                "fork: Failed to copy thread from current process, current pid: [{:?}], new pid: [{:?}]. Error: {:?}",
                current_pcb.raw_pid(), pcb.raw_pid(), e
            )
        });

        // 继承 rlimit
        pcb.inherit_rlimits_from(current_pcb);

        // 继承 executable_path
        // 修复：fork时需要复制父进程的可执行文件路径，而不是使用进程名
        // 这样才能正确支持通过/proc/self/exe重新执行程序
        pcb.set_execute_path(current_pcb.execute_path());

        // 继承 cmdline（/proc/<pid>/cmdline 语义）
        pcb.set_cmdline_bytes(current_pcb.cmdline_bytes());

        // alloc_pid
        if pcb.raw_pid() == RawPid::UNASSIGNED {
            // 分层PID分配：在父进程的子PID namespace中为新任务分配PID
            let ns = pcb.nsproxy().pid_namespace_for_children().clone();

            let main_pid_arc = alloc_pid(&ns)?;

            // 根namespace中的PID号作为RawPid
            let root_pid_nr = main_pid_arc
                .first_upid()
                .ok_or(SystemError::EINVAL)?
                .nr
                .data();
            // log::debug!("fork: root_pid_nr: {}", root_pid_nr);

            unsafe {
                pcb.force_set_raw_pid(RawPid(root_pid_nr));
            }
            pcb.init_task_pid(PidType::PID, main_pid_arc);
        }

        // log::debug!("fork: clone_flags: {:?}", clone_flags);
        // 设置线程组id、组长
        if clone_flags.contains(CloneFlags::CLONE_THREAD) {
            pcb.thread.write_irqsave().group_leader =
                current_pcb.thread.read_irqsave().group_leader.clone();
            unsafe {
                let ptr = pcb.as_ref() as *const ProcessControlBlock as *mut ProcessControlBlock;
                (*ptr).tgid = current_pcb.tgid;
            }
        } else {
            pcb.thread.write_irqsave().group_leader = Arc::downgrade(pcb);

            let ptr: *mut ProcessControlBlock =
                pcb.as_ref() as *const ProcessControlBlock as *mut ProcessControlBlock;
            unsafe {
                (*ptr).tgid = pcb.raw_pid();
            }
        }

        let current_leader = {
            let ti = current_pcb.thread.read_irqsave();
            ti.group_leader().unwrap_or_else(|| current_pcb.clone())
        };

        let clone_into_cgroup_target = Self::resolve_clone_into_cgroup_target(&clone_args)?;
        let reserved_cgroup = if pcb.raw_pid() > RawPid(0) {
            let charge_node = clone_into_cgroup_target
                .as_ref()
                .unwrap_or(&pcb.task_cgroup_node())
                .clone();
            let src_node = pcb.task_cgroup_node();
            let guard = cgroup_accounting_lock().lock();
            cgroup_can_fork_in(&charge_node, 1)?;
            if let Some(target_node) = clone_into_cgroup_target {
                cgroup_migrate_vet_dst_with_src(&src_node, &target_node, 1)?;
                pcb.set_task_cgroup_node_for_fork(target_node);
            }
            let cgroup = pcb.task_cgroup_node();
            cgroup.charge_pids(1);
            drop(guard);
            Some(cgroup)
        } else {
            None
        };

        let mut reserved_pidfd: Option<ReservedFd> = None;
        let mut pidfd_file: Option<File> = None;
        if clone_flags.contains(CloneFlags::CLONE_PIDFD) {
            let pid = pcb.pid();
            let prepared = match PidFd::prepare(current_pcb, pid, FileFlags::empty(), false) {
                Ok(prepared) => prepared,
                Err(err) => {
                    Self::rollback_failed_fork(current_pcb, None, reserved_cgroup.as_ref());
                    return Err(err);
                }
            };
            let fd = prepared.reservation.fd();

            let write_pidfd_result = (|| -> Result<(), SystemError> {
                let mut writer = UserBufferWriter::new(
                    clone_args.pidfd.data() as *mut i32,
                    core::mem::size_of::<i32>(),
                    true,
                )?;
                writer.copy_one_to_user(&(fd as i32), 0)
            })();
            if let Err(err) = write_pidfd_result {
                current_pcb
                    .fd_table()
                    .write()
                    .release_reserved_fd(prepared.reservation);
                Self::rollback_failed_fork(current_pcb, None, reserved_cgroup.as_ref());
                return Err(err);
            }

            reserved_pidfd = Some(prepared.reservation);
            pidfd_file = Some(prepared.file);
        }

        // 新任务的默认落点 CPU 应在 wake_up_new_task() 时再选择；这里只保留显式 hint，
        // 以避免 fork 长路径内父任务迁移导致的“过早采样当前 CPU”问题。
        pcb.sched_info().mark_new_task(clone_args.target_cpu);
        sched_cgroup_fork(pcb);

        // 处理 rseq 状态。按 Linux copy_process() 顺序，应在任务对外可见前完成。
        crate::process::rseq::rseq_fork(pcb, clone_flags.contains(CloneFlags::CLONE_VM));

        let publish_result: Result<(), SystemError> = {
            let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
            if clone_flags.contains(CloneFlags::CLONE_THREAD) {
                let inherited_parent = current_pcb.parent_pcb.read_irqsave().clone();
                let inherited_real_parent = current_pcb.real_parent_pcb.read_irqsave().clone();
                *pcb.parent_pcb.write_irqsave() = inherited_parent;
                *pcb.real_parent_pcb.write_irqsave() = inherited_real_parent.clone();
                *pcb.wait_parent_pcb.write_irqsave() = inherited_real_parent.clone();
                *pcb.fork_parent_pcb.write_irqsave() = inherited_real_parent;
                pcb.exit_signal.store(-1, Ordering::SeqCst);

                let group_leader = pcb.threads_read_irqsave().group_leader().unwrap();
                current_pcb.sighand().with_group_exec_check(|| {
                    pcb.attach_pid(PidType::PID);
                    pcb.task_join_group_stop();
                    group_leader
                        .threads_write_irqsave()
                        .group_tasks
                        .push(Arc::downgrade(pcb));
                })?;

                let leader_tgid_pid = group_leader.pid();
                pcb.init_task_pid(PidType::TGID, leader_tgid_pid.clone());
                pcb.init_task_pid(PidType::PGID, leader_tgid_pid.clone());
                pcb.init_task_pid(PidType::SID, leader_tgid_pid.clone());
                pcb.attach_pid(PidType::TGID);
                pcb.attach_pid(PidType::PGID);
                pcb.attach_pid(PidType::SID);
                Ok(())
            } else {
                if clone_flags.contains(CloneFlags::CLONE_PARENT) {
                    let inherited_parent = current_pcb.parent_pcb.read_irqsave().clone();
                    let inherited_real_parent = current_pcb.real_parent_pcb.read_irqsave().clone();
                    *pcb.parent_pcb.write_irqsave() = inherited_parent;
                    *pcb.real_parent_pcb.write_irqsave() = inherited_real_parent.clone();
                    *pcb.wait_parent_pcb.write_irqsave() = inherited_real_parent.clone();
                    *pcb.fork_parent_pcb.write_irqsave() = inherited_real_parent;
                    pcb.exit_signal.store(
                        current_leader.exit_signal.load(Ordering::SeqCst),
                        Ordering::SeqCst,
                    );
                } else {
                    *pcb.parent_pcb.write_irqsave() = Arc::downgrade(&current_leader);
                    *pcb.real_parent_pcb.write_irqsave() = Arc::downgrade(&current_leader);
                    *pcb.wait_parent_pcb.write_irqsave() = Arc::downgrade(current_pcb);
                    *pcb.fork_parent_pcb.write_irqsave() = Arc::downgrade(current_pcb);
                    pcb.exit_signal
                        .store(clone_args.exit_signal, Ordering::SeqCst);
                }

                if let Some(parent) = pcb.parent_pcb() {
                    let ppid_in_child_ns = parent
                        .task_pid_nr_ns(PidType::PID, Some(pcb.active_pid_ns()))
                        .unwrap_or(RawPid::new(0));
                    pcb.basic.write_irqsave().ppid = ppid_in_child_ns;
                }

                let pid = pcb.pid();
                if pcb.raw_pid() == RawPid(1) {
                    pcb.init_task_pid(PidType::TGID, pid.clone());
                    pcb.init_task_pid(PidType::PGID, pid.clone());
                    pcb.init_task_pid(PidType::SID, pid.clone());
                } else {
                    pcb.init_task_pid(PidType::TGID, pid.clone());
                    pcb.init_task_pid(PidType::PGID, current_pcb.task_pgrp().unwrap());
                    pcb.init_task_pid(PidType::SID, current_pcb.task_session().unwrap());
                }

                if pid.is_child_reaper() {
                    pid.ns_of_pid().set_child_reaper(Arc::downgrade(pcb));
                    pcb.sighand().flags_insert(SignalFlags::UNKILLABLE);
                }

                let real_parent = pcb
                    .real_parent_pcb()
                    .unwrap_or_else(|| current_leader.clone());
                let parent_leader = {
                    let ti = real_parent.thread.read_irqsave();
                    ti.group_leader().unwrap_or_else(|| real_parent.clone())
                };
                let parent_siginfo = parent_leader.sig_info_irqsave();
                let parent_tty = parent_siginfo.tty();
                let parent_has_child_subreaper = parent_siginfo.has_child_subreaper();
                let parent_is_child_reaper = parent_siginfo.is_child_subreaper();
                drop(parent_siginfo);
                let mut sig_info_guard = pcb.sig_info_mut();
                sig_info_guard.set_tty(parent_tty);
                sig_info_guard
                    .set_has_child_subreaper(parent_has_child_subreaper || parent_is_child_reaper);
                drop(sig_info_guard);

                pcb.attach_pid(PidType::TGID);
                pcb.attach_pid(PidType::PGID);
                pcb.attach_pid(PidType::SID);
                pcb.attach_pid(PidType::PID);

                // Publish the child into the parent's children list. This must
                // happen inside the same relation-lock critical section as parent
                // field init and PID attach, to close the window where exit/adopt
                // sees a parent pointer but the children list is still empty.
                if pcb.raw_pid() > RawPid(1) {
                    let parent = pcb.parent_pcb().unwrap_or_else(|| current_leader.clone());
                    let parent_leader = {
                        let ti = parent.thread.read_irqsave();
                        ti.group_leader().unwrap_or_else(|| parent.clone())
                    };
                    let mut children = parent_leader.children.write_irqsave();
                    let parent_ns = parent_leader.active_pid_ns();
                    let child_vpid = pcb.task_pid_nr_ns(PidType::PID, Some(parent_ns));
                    if let Some(vpid) = child_vpid {
                        if vpid.data() != 0 {
                            children.push(vpid);
                        } else {
                            warn!(
                                "fork: child pid is 0 in parent pidns, parent pid={:?}, child pid={:?}",
                                parent_leader.raw_pid(),
                                pcb.raw_pid()
                            );
                        }
                    } else {
                        warn!(
                            "fork: failed to resolve child pid in parent pidns, parent pid={:?}, child pid={:?}",
                            parent_leader.raw_pid(),
                            pcb.raw_pid()
                        );
                    }
                }

                Ok(())
            }
        };
        if let Err(err) = publish_result {
            if let Some(reservation) = reserved_pidfd.take() {
                current_pcb
                    .fd_table()
                    .write()
                    .release_reserved_fd(reservation);
            }
            Self::rollback_failed_fork(current_pcb, None, reserved_cgroup.as_ref());
            return Err(err);
        }

        if let (Some(reservation), Some(file)) = (reserved_pidfd.take(), pidfd_file.take()) {
            current_pcb
                .fd_table()
                .write()
                .install_reserved_fd(reservation, file)?;
        }

        if pcb.raw_pid() > RawPid(0) {
            let cgroup = pcb.task_cgroup_node();
            let needs_oom_score_adj_sync = Self::needs_oom_score_adj_clone_vm_sync(&clone_flags);
            let oom_score_adj_guard = if needs_oom_score_adj_sync {
                Some(ProcessManager::lock_oom_score_adj())
            } else {
                None
            };
            if needs_oom_score_adj_sync {
                Self::sync_oom_score_adj_for_clone_vm(current_pcb, pcb);
            }
            ProcessManager::add_pcb(pcb.clone(), &fs_refs_copy);
            drop(oom_score_adj_guard);
            cgroup.add_task(pcb.raw_pid());
            pcb.mark_visible_thread_accounted();
            inc_visible_thread_count();
            account_successful_fork();
        }
        drop(fs_refs_copy);

        // 设置child_tid，意味着子线程能够知道自己的id。
        // 按 Linux schedule_tail 语义，在子任务首次运行时再 best-effort 写入。
        if clone_flags.contains(CloneFlags::CLONE_CHILD_SETTID) {
            pcb.thread.write_irqsave().set_child_tid = Some(clone_args.child_tid);
        }

        Ok(())
    }

    fn rollback_failed_fork(
        current_pcb: &Arc<ProcessControlBlock>,
        installed_pidfd: Option<i32>,
        reserved_cgroup: Option<&Arc<crate::cgroup::CgroupNode>>,
    ) {
        if let Some(fd) = installed_pidfd {
            let dropped = {
                let fd_table = current_pcb.fd_table();
                let mut fd_table_guard = fd_table.write();
                fd_table_guard.drop_fd(fd)
            };
            match dropped {
                Ok(dropped) => {
                    if let Err(err) = dropped.finish_close() {
                        warn!("fork: failed to close rolled back pidfd: {:?}", err);
                    }
                }
                Err(err) => {
                    warn!("fork: failed to roll back pidfd {}: {:?}", fd, err);
                }
            }
        }

        if let Some(cgroup) = reserved_cgroup {
            cgroup.uncharge_pids(1);
        }
    }

    fn copy_fs(
        clone_flags: &CloneFlags,
        parent_pcb: &Arc<ProcessControlBlock>,
        child_pcb: &Arc<ProcessControlBlock>,
        fs_refs: &FsRefsReadGuard,
    ) -> Result<(), SystemError> {
        let fs = parent_pcb.fs_struct();
        let child_fs = if clone_flags.contains(CloneFlags::CLONE_FS) {
            fs
        } else {
            Arc::new((*fs).clone())
        };
        child_pcb.set_fs_struct(child_fs, fs_refs);
        Ok(())
    }

    fn resolve_clone_into_cgroup_target(
        clone_args: &KernelCloneArgs,
    ) -> Result<Option<Arc<crate::cgroup::CgroupNode>>, SystemError> {
        if !clone_args.flags.contains(CloneFlags::CLONE_INTO_CGROUP) {
            return Ok(None);
        }

        if clone_args.cgroup < 0 {
            return Err(SystemError::EBADF);
        }

        let current = ProcessManager::current_pcb();

        let fd = clone_args.cgroup;
        let file = current
            .fd_table()
            .read()
            .get_file_by_fd(fd)
            .ok_or(SystemError::EBADF)?;

        if file.with_io_fs(|fs| fs.name() != "cgroup2") {
            return Err(SystemError::EINVAL);
        }
        if file.inode().metadata()?.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        let node = cgroup2_inode_to_node(&file.inode())?;
        let ns_root = current.nsproxy().cgroup_ns.root_cgroup().clone();
        if !ns_root.is_ancestor_of(&node) {
            return Err(SystemError::ENOENT);
        }
        let src = current.task_cgroup_node();
        if !ns_root.is_ancestor_of(&src) {
            return Err(SystemError::ENOENT);
        }
        if Arc::ptr_eq(&src, &node) {
            return Ok(None);
        }
        if clone_args.flags.contains(CloneFlags::CLONE_THREAD) {
            return Err(SystemError::EINVAL);
        }
        file.with_io_fs(|fs| cgroup2_check_attach_permissions(fs.root_inode(), &src, &node))?;
        cgroup_migrate_vet_dst_with_src(&src, &node, 1)?;

        Ok(Some(node))
    }
}

impl ProcessControlBlock {
    /// https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/fork.c#1959
    pub(super) fn init_task_pid(&self, pid_type: PidType, pid: Arc<Pid>) {
        // log::debug!(
        //     "init_task_pid: pid_type: {:?}, raw_pid:{}",
        //     pid_type,
        //     self.raw_pid().data()
        // );
        if pid_type == PidType::PID {
            self.thread_pid.write().replace(pid);
        } else {
            self.sighand().set_pid(pid_type, Some(pid));
        }
    }
}
