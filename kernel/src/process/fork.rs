use alloc::vec::Vec;
use core::{intrinsics::unlikely, sync::atomic::Ordering};

use crate::arch::MMArch;
use crate::filesystem::vfs::file::File;
use crate::filesystem::vfs::file::FileMode;
use crate::filesystem::vfs::file::FilePrivateData;
use crate::filesystem::vfs::syscall::ModeType;
use crate::filesystem::vfs::FileType;
use crate::mm::verify_area;
use crate::mm::MemoryManagementArch;
use crate::process::pid::PidPrivateData;
use alloc::{string::ToString, sync::Arc};
use log::error;
use system_error::SystemError;

use crate::{
    arch::{interrupt::TrapFrame, ipc::signal::Signal},
    ipc::signal_types::SignalFlags,
    libs::rwlock::RwLock,
    mm::VirtAddr,
    process::ProcessFlags,
    sched::{sched_cgroup_fork, sched_fork},
    smp::core::smp_get_processor_id,
    syscall::user_access::UserBufferWriter,
};

use super::{
    alloc_pid,
    kthread::{KernelThreadPcbPrivate, WorkerPrivate},
    pid::{Pid, PidType},
    KernelStack, ProcessControlBlock, ProcessManager, RawPid,
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

    // 下列属性均来自用户空间
    pub pidfd: VirtAddr,
    pub child_tid: VirtAddr,
    pub parent_tid: VirtAddr,
    pub set_tid: Vec<usize>,

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
            set_tid: Vec::with_capacity(MAX_PID_NS_LEVEL),
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

    pub fn verify(&self) -> Result<(), SystemError> {
        if self.flags.contains(CloneFlags::CLONE_SETTLS) {
            verify_area(VirtAddr::new(self.tls), MMArch::PAGE_SIZE)
                .map_err(|_| SystemError::EPERM)?;
        }
        Ok(())
    }

    /// 规范化 exit_signal 字段，根据 Linux clone 语义处理
    ///
    /// ## 规则
    ///
    /// 1. 如果设置了 CLONE_THREAD，进程是线程组成员，不应发送 exit_signal（设为 INVALID）
    /// 2. 其他情况保持 exit_signal 不变
    ///
    /// 这个方法应该在 do_clone() 之前调用，确保 exit_signal 的语义正确。
    pub fn normalize_exit_signal(&mut self) {
        if self.flags.contains(CloneFlags::CLONE_THREAD) {
            // 线程组成员不发送 exit_signal
            self.exit_signal = Signal::INVALID;
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
        let current_pcb = ProcessManager::current_pcb();

        let new_kstack: KernelStack = KernelStack::new()?;

        let name = current_pcb.basic().name().to_string();

        let mut args = KernelCloneArgs::new();
        args.flags = clone_flags;
        args.exit_signal = Signal::SIGCHLD;
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

        pcb.sched_info().set_on_cpu(Some(smp_get_processor_id()));

        ProcessManager::wakeup(&pcb).unwrap_or_else(|e| {
            panic!(
                "fork: Failed to wakeup new process, pid: [{:?}]. Error: {:?}",
                pcb.raw_pid(),
                e
            )
        });

        if ProcessManager::current_pid().data() == 0 {
            return Ok(pcb.raw_pid());
        }

        return Ok(pcb.pid().pid_vnr());
    }

    fn copy_flags(
        clone_flags: &CloneFlags,
        new_pcb: &Arc<ProcessControlBlock>,
    ) -> Result<(), SystemError> {
        if clone_flags.contains(CloneFlags::CLONE_VFORK) {
            new_pcb.flags().insert(ProcessFlags::VFORK);
        }
        *new_pcb.flags.get_mut() = *ProcessManager::current_pcb().flags();
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
                current_pcb.raw_pid()
            )
        });

        if clone_flags.contains(CloneFlags::CLONE_VM) {
            unsafe { new_pcb.basic_mut().set_user_vm(Some(old_address_space)) };
            return Ok(());
        }
        let new_address_space = old_address_space.write_irqsave().try_clone().unwrap_or_else(|e| {
            panic!(
                "copy_mm: Failed to clone address space of current process, current pid: [{:?}], new pid: [{:?}]. Error: {:?}",
                current_pcb.raw_pid(), new_pcb.raw_pid(), e
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
            let new_fd_table = current_pcb.basic().try_fd_table().unwrap().read().clone();
            let new_fd_table = Arc::new(RwLock::new(new_fd_table));
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

        if (clone_flags & (CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_FS)
            == (CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_FS))
            || (clone_flags & (CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_FS))
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
            && !((clone_flags & (CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_NEWPID)).is_empty())
        {
            return Err(SystemError::EINVAL);
            // TODO: 判断新进程与当前进程namespace是否相同，不同则返回错误
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

        // 标记当前线程还未被执行exec
        pcb.flags().insert(ProcessFlags::FORKNOEXEC);

        // 克隆 pidfd
        if clone_flags.contains(CloneFlags::CLONE_PIDFD) {
            let pid = pcb.raw_pid().0 as i32;
            let root_inode = ProcessManager::current_mntns().root_inode();
            let name = format!(
                "Pidfd(from {} to {})",
                ProcessManager::current_pcb().raw_pid().data(),
                pid
            );
            let new_inode =
                root_inode.create(&name, FileType::File, ModeType::from_bits_truncate(0o777))?;
            let file = File::new(new_inode, FileMode::O_RDWR | FileMode::O_CLOEXEC)?;
            {
                let mut guard = file.private_data.lock();
                *guard = FilePrivateData::Pid(PidPrivateData::new(pid));
            }
            let r = current_pcb
                .fd_table()
                .write()
                .alloc_fd(file, None)
                .map(|fd| fd as usize);

            let mut writer = UserBufferWriter::new(
                clone_args.parent_tid.data() as *mut i32,
                core::mem::size_of::<i32>(),
                true,
            )?;

            writer.copy_one_to_user(&(r.unwrap() as i32), 0)?;
        }

        sched_fork(pcb).unwrap_or_else(|e| {
            panic!(
                "fork: Failed to set sched info from current process, current pid: [{:?}], new pid: [{:?}]. Error: {:?}",
                current_pcb.raw_pid(), pcb.raw_pid(), e
            )
        });

        // 拷贝标志位
        Self::copy_flags(&clone_flags, pcb).unwrap_or_else(|e| {
            panic!(
                "fork: Failed to copy flags from current process, current pid: [{:?}], new pid: [{:?}]. Error: {:?}",
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

        // 拷贝文件描述符表
        Self::copy_files(&clone_flags, current_pcb, pcb).unwrap_or_else(|e| {
            panic!(
                "fork: Failed to copy files from current process, current pid: [{:?}], new pid: [{:?}]. Error: {:?}",
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

        // 拷贝信号备用栈
        Self::copy_sigaltstack(&clone_flags, current_pcb, pcb).unwrap_or_else(|e| {
            panic!(
                "fork: Failed to copy sigaltstack from current process, current pid: [{:?}], new pid: [{:?}]. Error: {:?}",
                current_pcb.raw_pid(), pcb.raw_pid(), e
            )
        });

        // 拷贝namespace
        Self::copy_namespaces(&clone_flags, current_pcb, pcb).unwrap_or_else(|e| {
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
                (*ptr).tgid = pcb.pid;
            }
        }

        // CLONE_PARENT re-uses the old parent
        if !((clone_flags & (CloneFlags::CLONE_PARENT | CloneFlags::CLONE_THREAD)).is_empty()) {
            *pcb.real_parent_pcb.write_irqsave() =
                current_pcb.real_parent_pcb.read_irqsave().clone();

            if clone_flags.contains(CloneFlags::CLONE_THREAD) {
                pcb.exit_signal.store(Signal::INVALID, Ordering::SeqCst);
            } else {
                let leader = current_pcb.thread.read_irqsave().group_leader();
                if unlikely(leader.is_none()) {
                    panic!(
                        "fork: Failed to get leader of current process, current pid: [{:?}]",
                        current_pcb.raw_pid()
                    );
                }

                pcb.exit_signal.store(
                    leader.unwrap().exit_signal.load(Ordering::SeqCst),
                    Ordering::SeqCst,
                );
            }
        } else {
            // 新创建的进程，设置其父进程为当前进程
            *pcb.real_parent_pcb.write_irqsave() = Arc::downgrade(current_pcb);
            pcb.exit_signal
                .store(clone_args.exit_signal, Ordering::SeqCst);
        }

        Self::copy_fs(&clone_flags, current_pcb, pcb).unwrap_or_else(|e| {
            panic!(
                "fork: Failed to copy fs from current process, current pid: [{:?}], new pid: [{:?}]. Error: {:?}",
                current_pcb.raw_pid(), pcb.raw_pid(), e
            )
        });

        if pcb.raw_pid() == RawPid::UNASSIGNED {
            // 分层PID分配：在父进程的子PID namespace中为新任务分配PID
            let ns = pcb.nsproxy().pid_namespace_for_children().clone();

            let main_pid_arc = alloc_pid(&ns).expect("alloc_pid failed");

            // 根namespace中的PID号作为RawPid
            let root_pid_nr = main_pid_arc
                .first_upid()
                .expect("UPid list empty")
                .nr
                .data();
            // log::debug!("fork: root_pid_nr: {}", root_pid_nr);

            unsafe {
                pcb.force_set_raw_pid(RawPid(root_pid_nr));
            }
            pcb.init_task_pid(PidType::PID, main_pid_arc);
        }

        // 将当前pcb加入父进程的子进程哈希表中
        if pcb.raw_pid() > RawPid(1) {
            if let Some(ppcb_arc) = pcb.parent_pcb.read_irqsave().upgrade() {
                let mut children = ppcb_arc.children.write_irqsave();
                children.push(pcb.raw_pid());
            } else {
                panic!("parent pcb is None");
            }
        }

        if pcb.raw_pid() > RawPid(0) {
            ProcessManager::add_pcb(pcb.clone());
        }

        let pid = pcb.pid();
        if pcb.is_thread_group_leader() {
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

            let parent_siginfo = current_pcb.sig_info_irqsave();
            let parent_tty = parent_siginfo.tty();
            let parent_has_child_subreaper = parent_siginfo.has_child_subreaper();
            let parent_is_child_reaper = parent_siginfo.is_child_subreaper();
            drop(parent_siginfo);
            let mut sig_info_guard = pcb.sig_info_mut();

            // log::debug!("set tty: {:?}", parent_tty);
            sig_info_guard.set_tty(parent_tty);

            /*
             * Inherit has_child_subreaper flag under the same
             * tasklist_lock with adding child to the process tree
             * for propagate_has_child_subreaper optimization.
             */
            sig_info_guard
                .set_has_child_subreaper(parent_has_child_subreaper || parent_is_child_reaper);
            drop(sig_info_guard);
            pcb.attach_pid(PidType::TGID);
            pcb.attach_pid(PidType::PGID);
            pcb.attach_pid(PidType::SID);
        } else {
            pcb.task_join_group_stop();
            let group_leader = pcb.threads_read_irqsave().group_leader().unwrap();
            group_leader
                .threads_write_irqsave()
                .group_tasks
                .push(Arc::downgrade(pcb));

            // 确保非组长线程的 TGID 与组长一致
            let leader_tgid_pid = group_leader.pid();
            pcb.init_task_pid(PidType::TGID, leader_tgid_pid.clone());
            pcb.init_task_pid(PidType::PGID, leader_tgid_pid.clone());
            pcb.init_task_pid(PidType::SID, leader_tgid_pid.clone());
            pcb.attach_pid(PidType::TGID);
            pcb.attach_pid(PidType::PGID);
            pcb.attach_pid(PidType::SID);
        }

        pcb.attach_pid(PidType::PID);

        // 将子进程/线程的id存储在用户态传进的地址中
        if clone_flags.contains(CloneFlags::CLONE_PARENT_SETTID) {
            let mut writer = UserBufferWriter::new(
                clone_args.parent_tid.data() as *mut i32,
                core::mem::size_of::<i32>(),
                true,
            )?;

            writer.copy_one_to_user(&(pcb.raw_pid().0 as i32), 0)?;
        }

        sched_cgroup_fork(pcb);

        Ok(())
    }

    fn copy_fs(
        clone_flags: &CloneFlags,
        parent_pcb: &Arc<ProcessControlBlock>,
        child_pcb: &Arc<ProcessControlBlock>,
    ) -> Result<(), SystemError> {
        let fs = parent_pcb.fs_struct();
        let mut guard = child_pcb.fs_struct_mut();
        if clone_flags.contains(CloneFlags::CLONE_FS) {
            *guard = fs.clone();
        } else {
            let new_fs = (*fs).clone();
            *guard = Arc::new(new_fs);
        }
        Ok(())
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
