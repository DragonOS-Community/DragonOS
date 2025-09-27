use core::{
    fmt,
    hash::Hash,
    hint::spin_loop,
    intrinsics::{likely, unlikely},
    mem::ManuallyDrop,
    sync::atomic::{compiler_fence, fence, AtomicBool, AtomicUsize, Ordering},
};

use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use cred::INIT_CRED;
use hashbrown::HashMap;
use intertrait::cast::CastArc;
use log::{debug, error, info, warn};
use pid::{alloc_pid, Pid, PidLink, PidType};
use process_group::Pgid;
use system_error::SystemError;

use crate::{
    arch::{
        cpu::current_cpu_id,
        ipc::signal::{AtomicSignal, SigSet, Signal},
        process::ArchPCBInfo,
        CurrentIrqArch,
    },
    driver::tty::tty_core::TtyCore,
    exception::InterruptArch,
    filesystem::{
        fs::FsStruct,
        procfs::{procfs_register_pid, procfs_unregister_pid},
        vfs::{file::FileDescriptorVec, FileType, IndexNode},
    },
    ipc::{
        sighand::SigHand,
        signal::RestartBlock,
        signal_types::{SigInfo, SigPending},
    },
    libs::{
        align::AlignedBox,
        futex::{
            constant::{FutexFlag, FUTEX_BITSET_MATCH_ANY},
            futex::{Futex, RobustListHead},
        },
        lock_free_flags::LockFreeFlags,
        rwlock::{RwLock, RwLockReadGuard, RwLockUpgradableGuard, RwLockWriteGuard},
        spinlock::{SpinLock, SpinLockGuard},
        wait_queue::WaitQueue,
    },
    mm::{
        percpu::{PerCpu, PerCpuVar},
        set_IDLE_PROCESS_ADDRESS_SPACE,
        ucontext::AddressSpace,
        PhysAddr, VirtAddr,
    },
    net::socket::Socket,
    process::resource::{RLimit64, RLimitID},
    sched::{
        DequeueFlag, EnqueueFlag, OnRq, SchedMode, WakeupFlags, __schedule, completion::Completion,
        cpu_rq, fair::FairSchedEntity, prio::MAX_PRIO,
    },
    smp::{
        core::smp_get_processor_id,
        cpu::{AtomicProcessorId, ProcessorId},
        kick_cpu,
    },
    syscall::user_access::clear_user,
};
use timer::AlarmTimer;

use self::{cred::Cred, kthread::WorkerPrivate};
use crate::process::namespace::nsproxy::NsProxy;

pub mod abi;
pub mod cred;
pub mod exec;
pub mod execve;
pub mod exit;
pub mod fork;
pub mod geteuid;
pub mod idle;
pub mod kthread;
pub mod namespace;
pub mod pid;
pub mod process_group;
pub mod resource;
pub mod session;
pub mod signal;
pub mod stdio;
pub mod syscall;
pub mod timer;
pub mod utils;

/// 系统中所有进程的pcb
static ALL_PROCESS: SpinLock<Option<HashMap<RawPid, Arc<ProcessControlBlock>>>> =
    SpinLock::new(None);

pub static mut PROCESS_SWITCH_RESULT: Option<PerCpuVar<SwitchResult>> = None;

/// 一个只改变1次的全局变量，标志进程管理器是否已经初始化完成
static mut __PROCESS_MANAGEMENT_INIT_DONE: bool = false;

pub struct SwitchResult {
    pub prev_pcb: Option<Arc<ProcessControlBlock>>,
    pub next_pcb: Option<Arc<ProcessControlBlock>>,
}

impl SwitchResult {
    pub fn new() -> Self {
        Self {
            prev_pcb: None,
            next_pcb: None,
        }
    }
}

#[derive(Debug)]
pub struct ProcessManager;
impl ProcessManager {
    #[inline(never)]
    fn init() {
        static INIT_FLAG: AtomicBool = AtomicBool::new(false);
        if INIT_FLAG
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            panic!("ProcessManager has been initialized!");
        }

        unsafe {
            compiler_fence(Ordering::SeqCst);
            debug!("To create address space for INIT process.");
            // test_buddy();
            set_IDLE_PROCESS_ADDRESS_SPACE(
                AddressSpace::new(true).expect("Failed to create address space for INIT process."),
            );
            debug!("INIT process address space created.");
            compiler_fence(Ordering::SeqCst);
        };

        ALL_PROCESS.lock_irqsave().replace(HashMap::new());
        Self::init_switch_result();
        Self::arch_init();
        debug!("process arch init done.");
        Self::init_idle();
        debug!("process idle init done.");

        unsafe { __PROCESS_MANAGEMENT_INIT_DONE = true };
        info!("Process Manager initialized.");
        
        // 进程管理系统初始化完成后，初始化 ProcFS 的进程相关功能
        Self::init_procfs_after_process_init();
    }

    fn init_switch_result() {
        let mut switch_res_vec: Vec<SwitchResult> = Vec::new();
        for _ in 0..PerCpu::MAX_CPU_NUM {
            switch_res_vec.push(SwitchResult::new());
        }
        unsafe {
            PROCESS_SWITCH_RESULT = Some(PerCpuVar::new(switch_res_vec).unwrap());
        }
    }

    /// 判断进程管理器是否已经初始化完成
    #[allow(dead_code)]
    pub fn initialized() -> bool {
        unsafe { __PROCESS_MANAGEMENT_INIT_DONE }
    }

    /// 在进程管理系统初始化完成后，初始化 ProcFS 的进程相关功能
    fn init_procfs_after_process_init() {
        // 旧的全局 PROCFS_INSTANCE 已移除。直接通过通知机制为当前进程注册到已挂载的 /proc 实例。
        let current_pid = ProcessManager::current_pid();
        if current_pid != RawPid(0) {
            if let Err(e) = procfs_register_pid(current_pid) {
                warn!("Failed to register current process {} to procfs: {:?}", current_pid, e);
            } else {
                info!("Successfully registered current process {} to procfs", current_pid);
            }
        }
    }

    /// 获取当前进程的pcb
    pub fn current_pcb() -> Arc<ProcessControlBlock> {
        if unlikely(unsafe { !__PROCESS_MANAGEMENT_INIT_DONE }) {
            error!("unsafe__PROCESS_MANAGEMENT_INIT_DONE == false");
            loop {
                spin_loop();
            }
        }
        return ProcessControlBlock::arch_current_pcb();
    }

    /// 获取当前进程的pid
    ///
    /// 如果进程管理器未初始化完成，那么返回0
    pub fn current_pid() -> RawPid {
        if unlikely(unsafe { !__PROCESS_MANAGEMENT_INIT_DONE }) {
            return RawPid(0);
        }

        return ProcessManager::current_pcb().raw_pid();
    }

    /// 增加当前进程的锁持有计数
    #[inline(always)]
    pub fn preempt_disable() {
        if likely(unsafe { __PROCESS_MANAGEMENT_INIT_DONE }) {
            ProcessManager::current_pcb().preempt_disable();
        }
    }

    /// 减少当前进程的锁持有计数
    #[inline(always)]
    pub fn preempt_enable() {
        if likely(unsafe { __PROCESS_MANAGEMENT_INIT_DONE }) {
            ProcessManager::current_pcb().preempt_enable();
        }
    }

    /// 根据pid获取进程的pcb
    ///
    /// ## 参数
    ///
    /// - `pid` : 进程的pid
    ///
    /// ## 返回值
    ///
    /// 如果找到了对应的进程，那么返回该进程的pcb，否则返回None
    pub fn find(pid: RawPid) -> Option<Arc<ProcessControlBlock>> {
        return ALL_PROCESS.lock_irqsave().as_ref()?.get(&pid).cloned();
    }

    /// 向系统中添加一个进程的pcb
    ///
    /// ## 参数
    ///
    /// - `pcb` : 进程的pcb
    ///
    /// ## 返回值
    ///
    /// 无
    pub fn add_pcb(pcb: Arc<ProcessControlBlock>) {
        ALL_PROCESS
            .lock_irqsave()
            .as_mut()
            .unwrap()
            .insert(pcb.raw_pid(), pcb.clone());
    }

    /// ### 获取所有进程的pid
    pub fn get_all_processes() -> Vec<RawPid> {
        let mut pids = Vec::new();
        for (pid, _) in ALL_PROCESS.lock_irqsave().as_ref().unwrap().iter() {
            pids.push(*pid);
        }
        pids
    }

    /// 唤醒一个进程
    pub fn wakeup(pcb: &Arc<ProcessControlBlock>) -> Result<(), SystemError> {
        let _guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        let state = pcb.sched_info().inner_lock_read_irqsave().state();
        if state.is_blocked() {
            let mut writer = pcb.sched_info().inner_lock_write_irqsave();
            let state = writer.state();
            if state.is_blocked() {
                writer.set_state(ProcessState::Runnable);
                writer.set_wakeup();

                // avoid deadlock
                drop(writer);

                let rq =
                    cpu_rq(pcb.sched_info().on_cpu().unwrap_or(current_cpu_id()).data() as usize);

                let (rq, _guard) = rq.self_lock();
                rq.update_rq_clock();
                rq.activate_task(
                    pcb,
                    EnqueueFlag::ENQUEUE_WAKEUP | EnqueueFlag::ENQUEUE_NOCLOCK,
                );

                rq.check_preempt_currnet(pcb, WakeupFlags::empty());

                // sched_enqueue(pcb.clone(), true);
                return Ok(());
            } else if state.is_exited() {
                return Err(SystemError::EINVAL);
            } else {
                return Ok(());
            }
        } else if state.is_exited() {
            return Err(SystemError::EINVAL);
        } else {
            return Ok(());
        }
    }

    /// 唤醒暂停的进程
    pub fn wakeup_stop(pcb: &Arc<ProcessControlBlock>) -> Result<(), SystemError> {
        let _guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        let state = pcb.sched_info().inner_lock_read_irqsave().state();
        if let ProcessState::Stopped = state {
            let mut writer = pcb.sched_info().inner_lock_write_irqsave();
            let state = writer.state();
            if let ProcessState::Stopped = state {
                writer.set_state(ProcessState::Runnable);
                // avoid deadlock
                drop(writer);

                let rq = cpu_rq(
                    pcb.sched_info()
                        .on_cpu()
                        .unwrap_or(smp_get_processor_id())
                        .data() as usize,
                );

                let (rq, _guard) = rq.self_lock();
                rq.update_rq_clock();
                rq.activate_task(
                    pcb,
                    EnqueueFlag::ENQUEUE_WAKEUP | EnqueueFlag::ENQUEUE_NOCLOCK,
                );

                rq.check_preempt_currnet(pcb, WakeupFlags::empty());

                // sched_enqueue(pcb.clone(), true);
                return Ok(());
            } else if state.is_runnable() {
                return Ok(());
            } else {
                return Err(SystemError::EINVAL);
            }
        } else if state.is_runnable() {
            return Ok(());
        } else {
            return Err(SystemError::EINVAL);
        }
    }

    /// 标志当前进程永久睡眠，但是发起调度的工作，应该由调用者完成
    ///
    /// ## 注意
    ///
    /// - 进入当前函数之前，不能持有sched_info的锁
    /// - 进入当前函数之前，必须关闭中断
    /// - 进入当前函数之后必须保证逻辑的正确性，避免被重复加入调度队列
    pub fn mark_sleep(interruptable: bool) -> Result<(), SystemError> {
        assert!(
            !CurrentIrqArch::is_irq_enabled(),
            "interrupt must be disabled before enter ProcessManager::mark_sleep()"
        );
        let pcb = ProcessManager::current_pcb();
        let mut writer = pcb.sched_info().inner_lock_write_irqsave();
        if !matches!(writer.state(), ProcessState::Exited(_)) {
            writer.set_state(ProcessState::Blocked(interruptable));
            writer.set_sleep();
            pcb.flags().insert(ProcessFlags::NEED_SCHEDULE);
            fence(Ordering::SeqCst);
            drop(writer);
            return Ok(());
        }
        return Err(SystemError::EINTR);
    }

    /// 标志当前进程为停止状态，但是发起调度的工作，应该由调用者完成
    ///
    /// ## 注意
    ///
    /// - 进入当前函数之前，不能持有sched_info的锁
    /// - 进入当前函数之前，必须关闭中断
    pub fn mark_stop() -> Result<(), SystemError> {
        assert!(
            !CurrentIrqArch::is_irq_enabled(),
            "interrupt must be disabled before enter ProcessManager::mark_stop()"
        );

        let pcb = ProcessManager::current_pcb();
        let mut writer = pcb.sched_info().inner_lock_write_irqsave();
        if !matches!(writer.state(), ProcessState::Exited(_)) {
            writer.set_state(ProcessState::Stopped);
            pcb.flags().insert(ProcessFlags::NEED_SCHEDULE);
            drop(writer);

            return Ok(());
        }
        return Err(SystemError::EINTR);
    }

    /// 当子进程退出后向父进程发送通知
    #[inline(never)]
    fn exit_notify() {
        let current = ProcessManager::current_pcb();
        // 让INIT进程收养所有子进程
        if current.raw_pid() != RawPid(1) {
            unsafe {
                current
                    .adopt_childen()
                    .unwrap_or_else(|e| panic!("adopte_childen failed: error: {e:?}"))
            };
            let r = current.parent_pcb.read_irqsave().upgrade();
            if r.is_none() {
                return;
            }
            let parent_pcb = r.unwrap();

            let r = crate::ipc::kill::kill_process_by_pcb(parent_pcb.clone(), Signal::SIGCHLD);
            if let Err(e) = r {
                warn!(
                    "failed to send kill signal to {:?}'s parent pcb {:?}: {:?}",
                    current.raw_pid(),
                    parent_pcb.raw_pid(),
                    e
                );
            }
            // todo: 这里还需要根据线程组的信息，决定信号的发送
        }
    }

    /// 退出当前进程
    ///
    /// ## 参数
    ///
    /// - `exit_code` : 进程的退出码
    ///
    /// ## 注意
    ///  对于正常退出的进程，状态码应该先左移八位，以便用户态读取的时候正常返回退出码；而对于被信号终止的进程，状态码则是最低七位，无需进行移位操作。
    ///
    ///  因此注意，传入的`exit_code`应该是已经完成了移位操作的
    pub fn exit(exit_code: usize) -> ! {
        // 检查是否是init进程尝试退出，如果是则产生panic
        let current_pcb = ProcessManager::current_pcb();
        if current_pcb.raw_pid() == RawPid(1) {
            log::error!(
                "Init process (pid=1) attempted to exit with code {}. This should not happen and indicates a serious system error.",
                exit_code
            );
            loop {
                spin_loop();
            }
        }
        drop(current_pcb);

        // 关中断
        let _irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };

        let pid: Arc<Pid>;
        let raw_pid = ProcessManager::current_pid();
        {
            let pcb = ProcessManager::current_pcb();
            pid = pcb.pid();
            pcb.sched_info
                .inner_lock_write_irqsave()
                .set_state(ProcessState::Exited(exit_code));
            pcb.wait_queue.mark_dead();
            pcb.wait_queue.wakeup_all(Some(ProcessState::Blocked(true)));

            let rq = cpu_rq(smp_get_processor_id().data() as usize);
            let (rq, guard) = rq.self_lock();
            rq.deactivate_task(
                pcb.clone(),
                DequeueFlag::DEQUEUE_SLEEP | DequeueFlag::DEQUEUE_NOCLOCK,
            );
            drop(guard);

            // 进行进程退出后的工作
            let thread = pcb.thread.write_irqsave();

            if let Some(addr) = thread.clear_child_tid {
                if Arc::strong_count(&pcb.basic().user_vm().expect("User VM Not found")) > 1 {
                    let _ = Futex::futex_wake(
                        addr,
                        FutexFlag::FLAGS_MATCH_NONE,
                        1,
                        FUTEX_BITSET_MATCH_ANY,
                    );
                }
                unsafe { clear_user(addr, core::mem::size_of::<i32>()).expect("clear tid failed") };
            }
            compiler_fence(Ordering::SeqCst);

            RobustListHead::exit_robust_list(pcb.clone());
            // 如果是vfork出来的进程，则需要处理completion
            if thread.vfork_done.is_some() {
                thread.vfork_done.as_ref().unwrap().complete_all();
            }
            drop(thread);
            unsafe { pcb.basic_mut().set_user_vm(None) };
            pcb.exit_files();
            // TODO 由于未实现进程组，tty记录的前台进程组等于当前进程，故退出前要置空
            // 后续相关逻辑需要在SYS_EXIT_GROUP系统调用中实现
            if let Some(tty) = pcb.sig_info_irqsave().tty() {
                // 临时解决方案！！！ 临时解决方案！！！ 引入进程组之后，要重写这个更新前台进程组的逻辑
                let mut g = tty.core().contorl_info_irqsave();
                if g.pgid == Some(pid) {
                    g.pgid = None;
                }
            }
            pcb.sig_info_mut().set_tty(None);
            drop(pcb);
            ProcessManager::exit_notify();
        }

        __schedule(SchedMode::SM_NONE);
        error!("raw_pid {raw_pid:?} exited but sched again!");
        #[allow(clippy::empty_loop)]
        loop {
            spin_loop();
        }
    }

    /// 从全局进程列表中删除一个进程
    ///
    /// # 参数
    ///
    /// - `pid` : 进程的**全局** pid
    pub(super) unsafe fn release(pid: RawPid) {
        let pcb = ProcessManager::find(pid);
        if pcb.is_some() {
            // log::debug!("release pid {}", pid);
            // let pcb = pcb.unwrap();
            // 判断该pcb是否在全局没有任何引用
            // TODO: 当前，pcb的Arc指针存在泄露问题，引用计数不正确，打算在接下来实现debug专用的Arc，方便调试，然后解决这个bug。
            //          因此目前暂时注释掉，使得能跑
            // if Arc::strong_count(&pcb) <= 2 {
            //     drop(pcb);
            //     ALL_PROCESS.lock().as_mut().unwrap().remove(&pid);
            // } else {
            //     // 如果不为1就panic
            //     let msg = format!("pcb '{:?}' is still referenced, strong count={}",pcb.pid(),  Arc::strong_count(&pcb));
            //     error!("{}", msg);
            //     panic!()
            // }

            ALL_PROCESS.lock_irqsave().as_mut().unwrap().remove(&pid);
        }
    }

    /// 上下文切换完成后的钩子函数
    unsafe fn switch_finish_hook() {
        // debug!("switch_finish_hook");
        let prev_pcb = PROCESS_SWITCH_RESULT
            .as_mut()
            .unwrap()
            .get_mut()
            .prev_pcb
            .take()
            .expect("prev_pcb is None");
        let next_pcb = PROCESS_SWITCH_RESULT
            .as_mut()
            .unwrap()
            .get_mut()
            .next_pcb
            .take()
            .expect("next_pcb is None");

        // 由于进程切换前使用了SpinLockGuard::leak()，所以这里需要手动释放锁
        fence(Ordering::SeqCst);

        prev_pcb.arch_info.force_unlock();
        fence(Ordering::SeqCst);

        next_pcb.arch_info.force_unlock();
        fence(Ordering::SeqCst);
    }

    /// 如果目标进程正在目标CPU上运行，那么就让这个cpu陷入内核态
    ///
    /// ## 参数
    ///
    /// - `pcb` : 进程的pcb
    #[allow(dead_code)]
    pub fn kick(pcb: &Arc<ProcessControlBlock>) {
        ProcessManager::current_pcb().preempt_disable();
        let cpu_id = pcb.sched_info().on_cpu();

        if let Some(cpu_id) = cpu_id {
            if pcb.raw_pid() == cpu_rq(cpu_id.data() as usize).current().raw_pid() {
                kick_cpu(cpu_id).expect("ProcessManager::kick(): Failed to kick cpu");
            }
        }

        ProcessManager::current_pcb().preempt_enable();
    }
}

/// 上下文切换的钩子函数,当这个函数return的时候,将会发生上下文切换
#[cfg(target_arch = "x86_64")]
#[inline(never)]
pub unsafe extern "sysv64" fn switch_finish_hook() {
    ProcessManager::switch_finish_hook();
}
#[cfg(target_arch = "riscv64")]
#[inline(always)]
pub unsafe fn switch_finish_hook() {
    ProcessManager::switch_finish_hook();
}

int_like!(RawPid, AtomicRawPid, usize, AtomicUsize);

impl fmt::Display for RawPid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl RawPid {
    /// 该RawPid暂未分配，待会会初始化它。
    /// 这个状态只应当出现在进程/线程创建的过程中
    pub const UNASSIGNED: RawPid = RawPid(usize::MAX - 1);
    pub const MAX_VALID: RawPid = RawPid(usize::MAX - 32);

    pub fn is_valid(&self) -> bool {
        self.0 >= Self::MAX_VALID.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessState {
    /// The process is running on a CPU or in a run queue.
    Runnable,
    /// The process is waiting for an event to occur.
    /// 其中的bool表示该等待过程是否可以被打断。
    /// - 如果该bool为true,那么，硬件中断/信号/其他系统事件都可以打断该等待过程，使得该进程重新进入Runnable状态。
    /// - 如果该bool为false,那么，这个进程必须被显式的唤醒，才能重新进入Runnable状态。
    Blocked(bool),
    /// 进程被信号终止
    Stopped,
    /// 进程已经退出，usize表示进程的退出码
    Exited(usize),
}

#[allow(dead_code)]
impl ProcessState {
    #[inline(always)]
    pub fn is_runnable(&self) -> bool {
        return matches!(self, ProcessState::Runnable);
    }

    #[inline(always)]
    pub fn is_blocked(&self) -> bool {
        return matches!(self, ProcessState::Blocked(_));
    }

    #[inline(always)]
    pub fn is_blocked_interruptable(&self) -> bool {
        return matches!(self, ProcessState::Blocked(true));
    }

    /// Returns `true` if the process state is [`Exited`].
    #[inline(always)]
    pub fn is_exited(&self) -> bool {
        return matches!(self, ProcessState::Exited(_));
    }

    /// Returns `true` if the process state is [`Stopped`].
    ///
    /// [`Stopped`]: ProcessState::Stopped
    #[inline(always)]
    pub fn is_stopped(&self) -> bool {
        matches!(self, ProcessState::Stopped)
    }

    /// Returns exit code if the process state is [`Exited`].
    #[inline(always)]
    pub fn exit_code(&self) -> Option<usize> {
        match self {
            ProcessState::Exited(code) => Some(*code),
            _ => None,
        }
    }
}

bitflags! {
    /// pcb的标志位
    pub struct ProcessFlags: usize {
        /// 当前pcb表示一个内核线程
        const KTHREAD = 1 << 0;
        /// 当前进程需要被调度
        const NEED_SCHEDULE = 1 << 1;
        /// 进程由于vfork而与父进程存在资源共享
        const VFORK = 1 << 2;
        /// 进程不可被冻结
        const NOFREEZE = 1 << 3;
        /// 进程正在退出
        const EXITING = 1 << 4;
        /// 进程由于接收到终止信号唤醒
        const WAKEKILL = 1 << 5;
        /// 进程由于接收到信号而退出.(Killed by a signal)
        const SIGNALED = 1 << 6;
        /// 进程需要迁移到其他cpu上
        const NEED_MIGRATE = 1 << 7;
        /// 随机化的虚拟地址空间，主要用于动态链接器的加载
        const RANDOMIZE = 1 << 8;
        /// 进程有未处理的信号（这是一个用于快速判断的标志位）
        /// 相当于Linux的TIF_SIGPENDING
        const HAS_PENDING_SIGNAL = 1 << 9;
        /// 进程需要恢复之前保存的信号掩码
        const RESTORE_SIG_MASK = 1 << 10;
        /// Forked but didn't exec
        const FORKNOEXEC = 1 << 11;
    }
}

impl ProcessFlags {
    pub const fn exit_to_user_mode_work(&self) -> Self {
        Self::from_bits_truncate(self.bits & (Self::HAS_PENDING_SIGNAL.bits))
    }

    /// 测试并清除标志位
    ///
    /// ## 参数
    ///
    /// - `rhs` : 需要测试并清除的标志位
    ///
    /// ## 返回值
    ///
    /// 如果标志位在清除前是置位的，则返回 `true`，否则返回 `false`
    pub const fn test_and_clear(&mut self, rhs: Self) -> bool {
        let r = (self.bits & rhs.bits) != 0;
        self.bits &= !rhs.bits;
        r
    }
}
#[derive(Debug)]
pub struct ProcessControlBlock {
    /// 当前进程的pid
    pid: RawPid,
    /// 当前进程的线程组id（这个值在同一个线程组内永远不变）
    tgid: RawPid,

    thread_pid: RwLock<Option<Arc<Pid>>>,
    /// PID链接数组
    pid_links: [PidLink; PidType::PIDTYPE_MAX],

    /// namespace代理
    nsproxy: RwLock<Arc<NsProxy>>,

    basic: RwLock<ProcessBasicInfo>,
    /// 当前进程的自旋锁持有计数
    preempt_count: AtomicUsize,

    flags: LockFreeFlags<ProcessFlags>,
    worker_private: SpinLock<Option<WorkerPrivate>>,
    /// 进程的内核栈
    kernel_stack: RwLock<KernelStack>,

    /// 系统调用栈
    syscall_stack: RwLock<KernelStack>,

    /// 与调度相关的信息
    sched_info: ProcessSchedulerInfo,
    /// 与处理器架构相关的信息
    arch_info: SpinLock<ArchPCBInfo>,
    /// 与信号处理相关的信息(似乎可以是无锁的)
    sig_info: RwLock<ProcessSignalInfo>,
    sighand: RwLock<Arc<SigHand>>,

    /// 退出信号S
    exit_signal: AtomicSignal,

    /// 父进程指针
    parent_pcb: RwLock<Weak<ProcessControlBlock>>,
    /// 真实父进程指针
    real_parent_pcb: RwLock<Weak<ProcessControlBlock>>,

    /// 子进程链表
    children: RwLock<Vec<RawPid>>,

    /// 等待队列
    wait_queue: WaitQueue,

    /// 线程信息
    thread: RwLock<ThreadInfo>,

    /// 进程文件系统的状态
    fs: RwLock<Arc<FsStruct>>,

    ///闹钟定时器
    alarm_timer: SpinLock<Option<AlarmTimer>>,

    /// 进程的robust lock列表
    robust_list: RwLock<Option<RobustListHead>>,

    /// 进程作为主体的凭证集
    cred: SpinLock<Arc<Cred>>,
    self_ref: Weak<ProcessControlBlock>,

    restart_block: SpinLock<Option<RestartBlock>>,

    /// 进程的可执行文件路径
    executable_path: RwLock<String>,
    /// 资源限制（rlimit）数组
    rlimits: RwLock<[RLimit64; RLimitID::Nlimits as usize]>,
}

impl ProcessControlBlock {
    /// Generate a new pcb.
    ///
    /// ## 参数
    ///
    /// - `name` : 进程的名字
    /// - `kstack` : 进程的内核栈
    ///
    /// ## 返回值
    ///
    /// 返回一个新的pcb
    pub fn new(name: String, kstack: KernelStack) -> Arc<Self> {
        return Self::do_create_pcb(name, kstack, false);
    }

    /// 创建一个新的idle进程
    ///
    /// 请注意，这个函数只能在进程管理初始化的时候调用。
    pub fn new_idle(cpu_id: u32, kstack: KernelStack) -> Arc<Self> {
        let name = format!("idle-{}", cpu_id);
        return Self::do_create_pcb(name, kstack, true);
    }

    /// # 函数的功能
    ///
    /// 返回此函数是否是内核进程
    ///
    /// # 返回值
    ///
    /// 若进程是内核进程则返回true 否则返回false
    pub fn is_kthread(&self) -> bool {
        return matches!(self.flags(), &mut ProcessFlags::KTHREAD);
    }

    #[inline(never)]
    fn do_create_pcb(name: String, kstack: KernelStack, is_idle: bool) -> Arc<Self> {
        // 初始化namespace代理
        let nsproxy = if is_idle {
            // idle进程使用root namespace
            NsProxy::new_root()
        } else {
            // 其他进程继承父进程的namespace
            ProcessManager::current_pcb().nsproxy().clone()
        };

        let (raw_pid, ppid, cwd, cred, tty): (
            RawPid,
            RawPid,
            String,
            Arc<Cred>,
            Option<Arc<TtyCore>>,
        ) = if is_idle {
            let cred = INIT_CRED.clone();
            (RawPid(0), RawPid(0), "/".to_string(), cred, None)
        } else {
            let ppid = ProcessManager::current_pcb().task_pid_vnr();
            let cred = ProcessManager::current_pcb().cred();
            if cred.cap_ambient != cred.cap_permitted || cred.cap_ambient != cred.cap_effective {
                todo!("create a new cred for child.")
                //     cred.cap_permitted = cred.cap_ambient;
                // cred.cap_effective = cred.cap_ambient;
            }

            let cwd = ProcessManager::current_pcb().basic().cwd();
            let tty = ProcessManager::current_pcb().sig_info_irqsave().tty();

            // Here, UNASSIGNED is used to represent an unallocated pid,
            // which will be allocated later in `copy_process`.
            let raw_pid = RawPid::UNASSIGNED;

            (raw_pid, ppid, cwd, cred, tty)
        };

        let basic_info = ProcessBasicInfo::new(ppid, name.clone(), cwd, None);
        let preempt_count = AtomicUsize::new(0);
        let flags = unsafe { LockFreeFlags::new(ProcessFlags::empty()) };

        let sched_info = ProcessSchedulerInfo::new(None);

        let ppcb: Weak<ProcessControlBlock> = ProcessManager::find_task_by_vpid(ppid)
            .map(|p| Arc::downgrade(&p))
            .unwrap_or_default();

        // 使用 Arc::new_cyclic 避免在栈上创建巨大的结构体
        let pcb = Arc::new_cyclic(|weak| {
            let arch_info = SpinLock::new(ArchPCBInfo::new(&kstack));

            let pcb = Self {
                pid: raw_pid,
                tgid: raw_pid,
                thread_pid: RwLock::new(None),
                pid_links: core::array::from_fn(|_| PidLink::default()),
                nsproxy: RwLock::new(nsproxy),
                basic: basic_info,
                preempt_count,
                flags,
                kernel_stack: RwLock::new(kstack),
                syscall_stack: RwLock::new(KernelStack::new().unwrap()),
                worker_private: SpinLock::new(None),
                sched_info,
                arch_info,
                sig_info: RwLock::new(ProcessSignalInfo::default()),
                sighand: RwLock::new(SigHand::new()),
                exit_signal: AtomicSignal::new(Signal::SIGCHLD),
                parent_pcb: RwLock::new(ppcb.clone()),
                real_parent_pcb: RwLock::new(ppcb),
                children: RwLock::new(Vec::new()),
                wait_queue: WaitQueue::default(),
                thread: RwLock::new(ThreadInfo::new()),
                fs: RwLock::new(Arc::new(FsStruct::new())),
                alarm_timer: SpinLock::new(None),
                robust_list: RwLock::new(None),
                cred: SpinLock::new(cred),
                self_ref: weak.clone(),
                restart_block: SpinLock::new(None),
                executable_path: RwLock::new(name),
                rlimits: RwLock::new(Self::default_rlimits()),
            };

            pcb.sig_info.write().set_tty(tty);

            // 初始化系统调用栈
            #[cfg(target_arch = "x86_64")]
            pcb.arch_info
                .lock()
                .init_syscall_stack(&pcb.syscall_stack.read());

            pcb
        });

        pcb.sched_info()
            .sched_entity()
            .force_mut()
            .set_pcb(Arc::downgrade(&pcb));
        // 设置进程的arc指针到内核栈和系统调用栈的最低地址处
        unsafe {
            pcb.kernel_stack
                .write()
                .set_pcb(Arc::downgrade(&pcb))
                .unwrap();

            pcb.syscall_stack
                .write()
                .set_pcb(Arc::downgrade(&pcb))
                .unwrap()
        };

        return pcb;
    }

    fn default_rlimits() -> [crate::process::resource::RLimit64; RLimitID::Nlimits as usize] {
        use crate::mm::ucontext::UserStack;
        use crate::process::resource::{RLimit64, RLimitID};

        let mut arr = [RLimit64 {
            rlim_cur: 0,
            rlim_max: 0,
        }; RLimitID::Nlimits as usize];

        // Linux 典型默认值：软限制1024，硬限制65536
        // 文件描述符表会根据RLIMIT_NOFILE自动扩容
        arr[RLimitID::Nofile as usize] = RLimit64 {
            rlim_cur: 1024,
            rlim_max: 65536,
        };

        arr[RLimitID::Stack as usize] = RLimit64 {
            rlim_cur: UserStack::DEFAULT_USER_STACK_SIZE as u64,
            rlim_max: UserStack::DEFAULT_USER_STACK_SIZE as u64,
        };

        arr[RLimitID::As as usize] = {
            let end = <crate::arch::MMArch as crate::mm::MemoryManagementArch>::USER_END_VADDR;
            RLimit64 {
                rlim_cur: end.data() as u64,
                rlim_max: end.data() as u64,
            }
        };
        arr[RLimitID::Rss as usize] = arr[RLimitID::As as usize];

        arr
    }

    #[inline(always)]
    pub fn get_rlimit(&self, res: RLimitID) -> crate::process::resource::RLimit64 {
        self.rlimits.read()[res as usize]
    }

    pub fn set_rlimit(
        &self,
        res: RLimitID,
        newv: crate::process::resource::RLimit64,
    ) -> Result<(), system_error::SystemError> {
        use system_error::SystemError;
        if newv.rlim_cur > newv.rlim_max {
            return Err(SystemError::EINVAL);
        }

        // 注意：允许RLIMIT_NOFILE设置为0，这是测试用例的预期行为
        // 当rlim_cur为0时，无法分配新的文件描述符，但现有fd仍可使用

        // 对于RLIMIT_NOFILE，检查是否超过系统实现的最大容量限制
        if res == RLimitID::Nofile {
            if newv.rlim_cur > FileDescriptorVec::MAX_CAPACITY as u64 {
                return Err(SystemError::EINVAL);
            }
            if newv.rlim_max > FileDescriptorVec::MAX_CAPACITY as u64 {
                return Err(SystemError::EINVAL);
            }
        }

        let cur = self.rlimits.read()[res as usize];
        if newv.rlim_max > cur.rlim_max {
            let cred = self.cred();
            if !cred.has_capability(crate::process::cred::CAPFlags::CAP_SYS_RESOURCE) {
                return Err(SystemError::EPERM);
            }
        }

        // 更新rlimit
        self.rlimits.write()[res as usize] = newv;

        // 如果是RLIMIT_NOFILE变化，调整文件描述符表
        if res == RLimitID::Nofile {
            if let Err(e) = self.adjust_fd_table_for_rlimit_change(newv.rlim_cur as usize) {
                // 如果调整失败，回滚rlimit设置
                self.rlimits.write()[res as usize] = cur;
                return Err(e);
            }
        }

        Ok(())
    }

    /// 继承父进程的全部rlimit
    pub fn inherit_rlimits_from(&self, parent: &Arc<ProcessControlBlock>) {
        let src = *parent.rlimits.read();
        *self.rlimits.write() = src;

        // 继承后调整文件描述符表以匹配新的RLIMIT_NOFILE
        let nofile_limit = src[RLimitID::Nofile as usize].rlim_cur as usize;
        if let Err(e) = self.adjust_fd_table_for_rlimit_change(nofile_limit) {
            // 如果调整失败，记录错误但不影响继承过程
            error!(
                "Failed to adjust fd table after inheriting rlimits: {:?}",
                e
            );
        }
    }

    /// 当RLIMIT_NOFILE变化时调整文件描述符表
    ///
    /// ## 参数
    /// - `new_rlimit_nofile`: 新的RLIMIT_NOFILE值
    ///
    /// ## 返回值
    /// - `Ok(())`: 调整成功
    /// - `Err(SystemError)`: 调整失败
    fn adjust_fd_table_for_rlimit_change(
        &self,
        new_rlimit_nofile: usize,
    ) -> Result<(), system_error::SystemError> {
        let fd_table = self.basic.read().try_fd_table().unwrap();
        let mut fd_table_guard = fd_table.write();
        fd_table_guard.adjust_for_rlimit_change(new_rlimit_nofile)
    }

    /// 返回当前进程的锁持有计数
    #[inline(always)]
    pub fn preempt_count(&self) -> usize {
        return self.preempt_count.load(Ordering::SeqCst);
    }

    /// 增加当前进程的锁持有计数
    #[inline(always)]
    pub fn preempt_disable(&self) {
        self.preempt_count.fetch_add(1, Ordering::SeqCst);
    }

    /// 减少当前进程的锁持有计数
    #[inline(always)]
    pub fn preempt_enable(&self) {
        self.preempt_count.fetch_sub(1, Ordering::SeqCst);
    }

    #[inline(always)]
    pub unsafe fn set_preempt_count(&self, count: usize) {
        self.preempt_count.store(count, Ordering::SeqCst);
    }

    #[inline(always)]
    pub fn contain_child(&self, pid: &RawPid) -> bool {
        let children = self.children.read();
        return children.contains(pid);
    }

    #[inline(always)]
    pub fn flags(&self) -> &mut ProcessFlags {
        return self.flags.get_mut();
    }

    /// 请注意，这个值能在中断上下文中读取，但不能被中断上下文修改
    /// 否则会导致死锁
    #[inline(always)]
    pub fn basic(&self) -> RwLockReadGuard<'_, ProcessBasicInfo> {
        return self.basic.read_irqsave();
    }

    #[inline(always)]
    pub fn set_name(&self, name: String) {
        self.basic.write().set_name(name);
    }

    #[inline(always)]
    pub fn basic_mut(&self) -> RwLockWriteGuard<'_, ProcessBasicInfo> {
        return self.basic.write_irqsave();
    }

    /// # 获取arch info的锁，同时关闭中断
    #[inline(always)]
    pub fn arch_info_irqsave(&self) -> SpinLockGuard<'_, ArchPCBInfo> {
        return self.arch_info.lock_irqsave();
    }

    /// # 获取arch info的锁，但是不关闭中断
    ///
    /// 由于arch info在进程切换的时候会使用到，
    /// 因此在中断上下文外，获取arch info 而不irqsave是不安全的.
    ///
    /// 只能在以下情况下使用这个函数：
    /// - 在中断上下文中（中断已经禁用），获取arch info的锁。
    /// - 刚刚创建新的pcb
    #[inline(always)]
    pub unsafe fn arch_info(&self) -> SpinLockGuard<'_, ArchPCBInfo> {
        return self.arch_info.lock();
    }

    #[inline(always)]
    pub fn kernel_stack(&self) -> RwLockReadGuard<'_, KernelStack> {
        return self.kernel_stack.read();
    }

    pub unsafe fn kernel_stack_force_ref(&self) -> &KernelStack {
        self.kernel_stack.force_get_ref()
    }

    #[inline(always)]
    #[allow(dead_code)]
    pub fn kernel_stack_mut(&self) -> RwLockWriteGuard<'_, KernelStack> {
        return self.kernel_stack.write();
    }

    #[inline(always)]
    pub fn sched_info(&self) -> &ProcessSchedulerInfo {
        return &self.sched_info;
    }

    #[inline(always)]
    pub fn worker_private(&self) -> SpinLockGuard<'_, Option<WorkerPrivate>> {
        return self.worker_private.lock();
    }

    #[inline(always)]
    pub fn raw_pid(&self) -> RawPid {
        return self.pid;
    }

    #[inline(always)]
    pub fn fs_struct(&self) -> Arc<FsStruct> {
        self.fs.read().clone()
    }

    pub fn fs_struct_mut(&self) -> RwLockWriteGuard<'_, Arc<FsStruct>> {
        self.fs.write()
    }

    pub fn pwd_inode(&self) -> Arc<dyn IndexNode> {
        self.fs.read().pwd()
    }

    /// 获取文件描述符表的Arc指针
    #[inline(always)]
    pub fn fd_table(&self) -> Arc<RwLock<FileDescriptorVec>> {
        return self.basic.read().try_fd_table().unwrap();
    }

    #[inline(always)]
    pub fn cred(&self) -> Arc<Cred> {
        self.cred.lock().clone()
    }

    /// 原子替换当前进程的凭据集（cred）
    ///
    /// - 使用 irqsave 写锁保证并发安全
    /// - 返回 Result 以便调用方在需要时扩展错误处理
    pub fn set_cred(&self, new: Arc<Cred>) -> Result<(), SystemError> {
        *self.cred.lock_irqsave() = new;
        Ok(())
    }

    pub fn set_execute_path(&self, path: String) {
        *self.executable_path.write() = path;
    }

    pub fn execute_path(&self) -> String {
        self.executable_path.read().clone()
    }

    pub fn real_parent_pcb(&self) -> Option<Arc<ProcessControlBlock>> {
        return self.real_parent_pcb.read_irqsave().upgrade();
    }

    /// 判断当前进程是否是全局的init进程
    pub fn is_global_init(&self) -> bool {
        self.task_tgid_vnr().unwrap() == RawPid(1)
    }

    /// 根据文件描述符序号，获取socket对象的Arc指针
    ///
    /// this is a helper function
    ///
    /// ## 参数
    ///
    /// - `fd` 文件描述符序号
    ///
    /// ## 返回值
    ///
    /// Option(&mut Box<dyn Socket>) socket对象的可变引用. 如果文件描述符不是socket，那么返回None
    pub fn get_socket(&self, fd: i32) -> Result<Arc<dyn Socket>, SystemError> {
        let f = ProcessManager::current_pcb()
            .fd_table()
            .read()
            .get_file_by_fd(fd)
            .ok_or({
                // log::warn!("get_socket: fd {} not found", fd);
                SystemError::EBADF
            })?;

        if f.file_type() != FileType::Socket {
            return Err(SystemError::EBADF);
        }
        // log::info!("get_socket: fd {} is a socket", fd);
        f.inode().cast::<dyn Socket>().map_err(|_| {
            log::error!("get_socket: fd {} is not a socket", fd);
            SystemError::EBADF
        })
    }

    /// 当前进程退出时,让初始进程收养所有子进程
    unsafe fn adopt_childen(&self) -> Result<(), SystemError> {
        match ProcessManager::find_task_by_vpid(RawPid(1)) {
            Some(init_pcb) => {
                let childen_guard = self.children.write();
                if Arc::ptr_eq(&self.self_ref.upgrade().unwrap(), &init_pcb) {
                    // 当前进程是namespace的init进程，由父进程所在的pidns的init进程去收养子进程
                    if let Some(parent_pcb) = self.real_parent_pcb() {
                        assert!(
                            !Arc::ptr_eq(&parent_pcb, &init_pcb),
                            "adopt_childen: parent_pcb is init_pcb, pid: {}",
                            self.raw_pid()
                        );
                        let parent_init = ProcessManager::find_task_by_pid_ns(
                            RawPid(1),
                            &parent_pcb.active_pid_ns(),
                        );
                        if parent_init.is_none() {
                            log::warn!(
                                "adopt_childen: parent_init is None, pid: {}",
                                self.raw_pid()
                            );
                            return Ok(());
                        }
                        let parent_init = parent_init.unwrap();
                        let mut parent_children_guard = parent_init.children.write();
                        childen_guard.iter().for_each(|pid| {
                            log::debug!(
                                "adopt_childen: pid {} is adopted by parent init pid {}",
                                pid,
                                parent_init.raw_pid()
                            );
                            parent_children_guard.push(*pid);
                        });

                        return Ok(());
                    } else {
                        log::warn!("adopt_childen: parent_pcb is None, pid: {}", self.raw_pid());
                        return Ok(());
                    }
                }
                let mut init_childen_guard = init_pcb.children.write();

                childen_guard.iter().for_each(|pid| {
                    log::debug!(
                        "adopt_childen: pid {} is adopted by init pid {}",
                        pid,
                        init_pcb.raw_pid()
                    );
                    init_childen_guard.push(*pid);
                });

                return Ok(());
            }
            _ => Err(SystemError::ECHILD),
        }
    }

    /// 生成进程的名字
    pub fn generate_name(program_path: &str) -> String {
        // Extract just the basename from the program path
        let name = program_path.split('/').next_back().unwrap_or(program_path);
        name.to_string()
    }

    pub fn sig_info_irqsave(&self) -> RwLockReadGuard<'_, ProcessSignalInfo> {
        self.sig_info.read_irqsave()
    }

    pub fn sig_info_upgradable(&self) -> RwLockUpgradableGuard<'_, ProcessSignalInfo> {
        self.sig_info.upgradeable_read_irqsave()
    }

    pub fn try_siginfo_irqsave(&self, times: u8) -> Option<RwLockReadGuard<'_, ProcessSignalInfo>> {
        for _ in 0..times {
            if let Some(r) = self.sig_info.try_read_irqsave() {
                return Some(r);
            }
        }

        return None;
    }

    pub fn sig_info_mut(&self) -> RwLockWriteGuard<'_, ProcessSignalInfo> {
        self.sig_info.write_irqsave()
    }

    pub fn try_siginfo_mut(&self, times: u8) -> Option<RwLockWriteGuard<'_, ProcessSignalInfo>> {
        for _ in 0..times {
            if let Some(r) = self.sig_info.try_write_irqsave() {
                return Some(r);
            }
        }

        return None;
    }

    /// 判断当前进程是否有未处理的信号
    pub fn has_pending_signal(&self) -> bool {
        let sig_info = self.sig_info_irqsave();
        let has_pending_thread = sig_info.sig_pending().has_pending();
        drop(sig_info);
        if has_pending_thread {
            return true;
        }
        // also check shared-pending in sighand
        let shared = self.sighand().shared_pending_signal();
        return !shared.is_empty();
    }

    /// 根据 pcb 的 flags 判断当前进程是否有未处理的信号
    pub fn has_pending_signal_fast(&self) -> bool {
        self.flags.get().contains(ProcessFlags::HAS_PENDING_SIGNAL)
    }

    /// 检查当前进程是否有未被阻塞的待处理信号。
    ///
    /// 注：该函数较慢，因此需要与 has_pending_signal_fast 一起使用。
    pub fn has_pending_not_masked_signal(&self) -> bool {
        let sig_info = self.sig_info_irqsave();
        let blocked: SigSet = *sig_info.sig_blocked();
        let mut pending: SigSet = sig_info.sig_pending().signal();
        drop(sig_info);
        pending.remove(blocked);
        // log::debug!(
        //     "pending and not masked:{:?}, masked: {:?}",
        //     pending,
        //     blocked
        // );
        let has_not_masked = !pending.is_empty();
        return has_not_masked;
    }

    #[inline(always)]
    pub fn get_robust_list(&self) -> RwLockReadGuard<'_, Option<RobustListHead>> {
        return self.robust_list.read_irqsave();
    }

    #[inline(always)]
    pub fn set_robust_list(&self, new_robust_list: Option<RobustListHead>) {
        *self.robust_list.write_irqsave() = new_robust_list;
    }

    pub fn alarm_timer_irqsave(&self) -> SpinLockGuard<'_, Option<AlarmTimer>> {
        return self.alarm_timer.lock_irqsave();
    }

    /// Exit fd table when process exit
    fn exit_files(&self) {
        // 关闭文件描述符表
        // 这里这样写的原因是避免某些inode在关闭时需要访问当前进程的basic，导致死锁
        let mut guard = self.basic.write_irqsave();
        let old = guard.set_fd_table(None);
        drop(guard);
        drop(old)
    }

    pub fn children_read_irqsave(&self) -> RwLockReadGuard<'_, Vec<RawPid>> {
        self.children.read_irqsave()
    }

    pub fn threads_read_irqsave(&self) -> RwLockReadGuard<'_, ThreadInfo> {
        self.thread.read_irqsave()
    }

    pub fn threads_write_irqsave(&self) -> RwLockWriteGuard<'_, ThreadInfo> {
        self.thread.write_irqsave()
    }

    pub fn restart_block(&self) -> SpinLockGuard<'_, Option<RestartBlock>> {
        self.restart_block.lock()
    }

    pub fn set_restart_fn(
        &self,
        restart_block: Option<RestartBlock>,
    ) -> Result<usize, SystemError> {
        *self.restart_block.lock() = restart_block;
        return Err(SystemError::ERESTART_RESTARTBLOCK);
    }

    pub fn parent_pcb(&self) -> Option<Arc<ProcessControlBlock>> {
        self.parent_pcb.read().upgrade()
    }

    pub fn is_exited(&self) -> bool {
        self.sched_info
            .inner_lock_read_irqsave()
            .state()
            .is_exited()
    }

    pub fn exit_code(&self) -> Option<usize> {
        self.sched_info
            .inner_lock_read_irqsave()
            .state()
            .exit_code()
    }

    /// 获取进程的namespace代理
    pub fn nsproxy(&self) -> Arc<NsProxy> {
        self.nsproxy.read().clone()
    }

    /// 设置进程的namespace代理
    ///
    /// ## 参数
    /// - `nsproxy` : 新的namespace代理
    ///
    /// ## 返回值
    /// 返回旧的namespace代理
    pub fn set_nsproxy(&self, nsproxy: Arc<NsProxy>) -> Arc<NsProxy> {
        let mut guard = self.nsproxy.write();
        let old = guard.clone();
        *guard = nsproxy;
        return old;
    }

    pub fn is_thread_group_leader(&self) -> bool {
        self.exit_signal.load(Ordering::SeqCst) != Signal::INVALID
    }
}

impl Drop for ProcessControlBlock {
    fn drop(&mut self) {
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        // log::debug!("Drop ProcessControlBlock: pid: {}", self.raw_pid(),);
        self.__exit_signal();
        // 在ProcFS中,解除进程的注册
        procfs_unregister_pid(self.raw_pid())
            .unwrap_or_else(|e: SystemError| panic!("procfs_unregister_pid failed: error: {e:?}"));
        if let Some(ppcb) = self.parent_pcb.read_irqsave().upgrade() {
            ppcb.children
                .write_irqsave()
                .retain(|pid| *pid != self.raw_pid());
        }

        // log::debug!("Drop pid: {:?}", self.pid());
        drop(irq_guard);
    }
}

/// 线程信息
#[derive(Debug)]
pub struct ThreadInfo {
    // 来自用户空间记录用户线程id的地址，在该线程结束时将该地址置0以通知父进程
    clear_child_tid: Option<VirtAddr>,
    set_child_tid: Option<VirtAddr>,

    vfork_done: Option<Arc<Completion>>,
    /// 线程组的组长
    group_leader: Weak<ProcessControlBlock>,

    /// 当前线程为组长时，该字段存储组内所有线程的pcb
    group_tasks: Vec<Weak<ProcessControlBlock>>,
}

impl Default for ThreadInfo {
    fn default() -> Self {
        Self::new()
    }
}

impl ThreadInfo {
    pub fn new() -> Self {
        Self {
            clear_child_tid: None,
            set_child_tid: None,
            vfork_done: None,
            group_leader: Weak::default(),
            group_tasks: Vec::new(),
        }
    }

    pub fn group_leader(&self) -> Option<Arc<ProcessControlBlock>> {
        return self.group_leader.upgrade();
    }

    pub fn thread_group_empty(&self) -> bool {
        let group_leader = self.group_leader();
        if let Some(leader) = group_leader {
            if Arc::ptr_eq(&leader, &ProcessManager::current_pcb()) {
                return self.group_tasks.is_empty();
            }
            return false;
        }
        return true;
    }
}

/// 进程的基本信息
///
/// 这个结构体保存进程的基本信息，主要是那些不会随着进程的运行而经常改变的信息。
#[derive(Debug)]
pub struct ProcessBasicInfo {
    /// 当前进程的父进程的pid
    ppid: RawPid,
    /// 进程的名字
    name: String,

    /// 当前进程的工作目录
    cwd: String,

    /// 用户地址空间
    user_vm: Option<Arc<AddressSpace>>,

    /// 文件描述符表
    fd_table: Option<Arc<RwLock<FileDescriptorVec>>>,
}

impl ProcessBasicInfo {
    #[inline(never)]
    pub fn new(
        ppid: RawPid,
        name: String,
        cwd: String,
        user_vm: Option<Arc<AddressSpace>>,
    ) -> RwLock<Self> {
        let fd_table = Arc::new(RwLock::new(FileDescriptorVec::new()));
        return RwLock::new(Self {
            ppid,
            name,
            cwd,
            user_vm,
            fd_table: Some(fd_table),
        });
    }

    pub fn ppid(&self) -> RawPid {
        return self.ppid;
    }

    pub fn name(&self) -> &str {
        return &self.name;
    }

    pub fn set_name(&mut self, name: String) {
        self.name = name;
    }

    pub fn cwd(&self) -> String {
        return self.cwd.clone();
    }
    pub fn set_cwd(&mut self, path: String) {
        return self.cwd = path;
    }

    pub fn user_vm(&self) -> Option<Arc<AddressSpace>> {
        return self.user_vm.clone();
    }

    pub unsafe fn set_user_vm(&mut self, user_vm: Option<Arc<AddressSpace>>) {
        self.user_vm = user_vm;
    }

    pub fn try_fd_table(&self) -> Option<Arc<RwLock<FileDescriptorVec>>> {
        return self.fd_table.clone();
    }

    pub fn set_fd_table(
        &mut self,
        fd_table: Option<Arc<RwLock<FileDescriptorVec>>>,
    ) -> Option<Arc<RwLock<FileDescriptorVec>>> {
        let old = self.fd_table.take();
        self.fd_table = fd_table;
        return old;
    }
}

#[derive(Debug)]
pub struct ProcessSchedulerInfo {
    /// 当前进程所在的cpu
    on_cpu: AtomicProcessorId,
    /// 如果当前进程等待被迁移到另一个cpu核心上（也就是flags中的PF_NEED_MIGRATE被置位），
    /// 该字段存储要被迁移到的目标处理器核心号
    // migrate_to: AtomicProcessorId,
    inner_locked: RwLock<InnerSchedInfo>,
    /// 进程的调度优先级
    // priority: SchedPriority,
    /// 当前进程的虚拟运行时间
    // virtual_runtime: AtomicIsize,
    /// 由实时调度器管理的时间片
    // rt_time_slice: AtomicIsize,
    pub sched_stat: RwLock<SchedInfo>,
    /// 调度策略
    pub sched_policy: RwLock<crate::sched::SchedPolicy>,
    /// cfs调度实体
    pub sched_entity: Arc<FairSchedEntity>,
    pub on_rq: SpinLock<OnRq>,

    pub prio_data: RwLock<PrioData>,
}

#[derive(Debug, Default)]
#[allow(dead_code)]
pub struct SchedInfo {
    /// 记录任务在特定 CPU 上运行的次数
    pub pcount: usize,
    /// 记录任务等待在运行队列上的时间
    pub run_delay: usize,
    /// 记录任务上次在 CPU 上运行的时间戳
    pub last_arrival: u64,
    /// 记录任务上次被加入到运行队列中的时间戳
    pub last_queued: u64,
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct PrioData {
    pub prio: i32,
    pub static_prio: i32,
    pub normal_prio: i32,
}

impl Default for PrioData {
    fn default() -> Self {
        Self {
            prio: MAX_PRIO - 20,
            static_prio: MAX_PRIO - 20,
            normal_prio: MAX_PRIO - 20,
        }
    }
}

#[derive(Debug)]
pub struct InnerSchedInfo {
    /// 当前进程的状态
    state: ProcessState,
    /// 进程的调度策略
    sleep: bool,
}

impl InnerSchedInfo {
    pub fn state(&self) -> ProcessState {
        return self.state;
    }

    pub fn set_state(&mut self, state: ProcessState) {
        self.state = state;
    }

    pub fn set_sleep(&mut self) {
        self.sleep = true;
    }

    pub fn set_wakeup(&mut self) {
        self.sleep = false;
    }

    pub fn is_mark_sleep(&self) -> bool {
        self.sleep
    }
}

impl ProcessSchedulerInfo {
    #[inline(never)]
    pub fn new(on_cpu: Option<ProcessorId>) -> Self {
        let cpu_id = on_cpu.unwrap_or(ProcessorId::INVALID);
        return Self {
            on_cpu: AtomicProcessorId::new(cpu_id),
            // migrate_to: AtomicProcessorId::new(ProcessorId::INVALID),
            inner_locked: RwLock::new(InnerSchedInfo {
                state: ProcessState::Blocked(false),
                sleep: false,
            }),
            // virtual_runtime: AtomicIsize::new(0),
            // rt_time_slice: AtomicIsize::new(0),
            // priority: SchedPriority::new(100).unwrap(),
            sched_stat: RwLock::new(SchedInfo::default()),
            sched_policy: RwLock::new(crate::sched::SchedPolicy::CFS),
            sched_entity: FairSchedEntity::new(),
            on_rq: SpinLock::new(OnRq::None),
            prio_data: RwLock::new(PrioData::default()),
        };
    }

    pub fn sched_entity(&self) -> Arc<FairSchedEntity> {
        return self.sched_entity.clone();
    }

    pub fn on_cpu(&self) -> Option<ProcessorId> {
        let on_cpu = self.on_cpu.load(Ordering::SeqCst);
        if on_cpu == ProcessorId::INVALID {
            return None;
        } else {
            return Some(on_cpu);
        }
    }

    pub fn set_on_cpu(&self, on_cpu: Option<ProcessorId>) {
        if let Some(cpu_id) = on_cpu {
            self.on_cpu.store(cpu_id, Ordering::SeqCst);
        } else {
            self.on_cpu.store(ProcessorId::INVALID, Ordering::SeqCst);
        }
    }

    // pub fn migrate_to(&self) -> Option<ProcessorId> {
    //     let migrate_to = self.migrate_to.load(Ordering::SeqCst);
    //     if migrate_to == ProcessorId::INVALID {
    //         return None;
    //     } else {
    //         return Some(migrate_to);
    //     }
    // }

    // pub fn set_migrate_to(&self, migrate_to: Option<ProcessorId>) {
    //     if let Some(data) = migrate_to {
    //         self.migrate_to.store(data, Ordering::SeqCst);
    //     } else {
    //         self.migrate_to
    //             .store(ProcessorId::INVALID, Ordering::SeqCst)
    //     }
    // }

    pub fn inner_lock_write_irqsave(&self) -> RwLockWriteGuard<'_, InnerSchedInfo> {
        return self.inner_locked.write_irqsave();
    }

    pub fn inner_lock_read_irqsave(&self) -> RwLockReadGuard<'_, InnerSchedInfo> {
        return self.inner_locked.read_irqsave();
    }

    // pub fn inner_lock_try_read_irqsave(
    //     &self,
    //     times: u8,
    // ) -> Option<RwLockReadGuard<InnerSchedInfo>> {
    //     for _ in 0..times {
    //         if let Some(r) = self.inner_locked.try_read_irqsave() {
    //             return Some(r);
    //         }
    //     }

    //     return None;
    // }

    // pub fn inner_lock_try_upgradable_read_irqsave(
    //     &self,
    //     times: u8,
    // ) -> Option<RwLockUpgradableGuard<InnerSchedInfo>> {
    //     for _ in 0..times {
    //         if let Some(r) = self.inner_locked.try_upgradeable_read_irqsave() {
    //             return Some(r);
    //         }
    //     }

    //     return None;
    // }

    // pub fn virtual_runtime(&self) -> isize {
    //     return self.virtual_runtime.load(Ordering::SeqCst);
    // }

    // pub fn set_virtual_runtime(&self, virtual_runtime: isize) {
    //     self.virtual_runtime
    //         .store(virtual_runtime, Ordering::SeqCst);
    // }
    // pub fn increase_virtual_runtime(&self, delta: isize) {
    //     self.virtual_runtime.fetch_add(delta, Ordering::SeqCst);
    // }

    // pub fn rt_time_slice(&self) -> isize {
    //     return self.rt_time_slice.load(Ordering::SeqCst);
    // }

    // pub fn set_rt_time_slice(&self, rt_time_slice: isize) {
    //     self.rt_time_slice.store(rt_time_slice, Ordering::SeqCst);
    // }

    // pub fn increase_rt_time_slice(&self, delta: isize) {
    //     self.rt_time_slice.fetch_add(delta, Ordering::SeqCst);
    // }

    pub fn policy(&self) -> crate::sched::SchedPolicy {
        return *self.sched_policy.read_irqsave();
    }
}

#[derive(Debug)]
pub struct KernelStack {
    stack: Option<AlignedBox<[u8; KernelStack::SIZE], { KernelStack::ALIGN }>>,
    /// 标记该内核栈是否可以被释放
    ty: KernelStackType,
}

#[derive(Debug)]
pub enum KernelStackType {
    KernelSpace(VirtAddr, PhysAddr),
    Static,
    Dynamic,
}

// 为什么需要这个锁?
// alloc_from_kernel_space 使用该函数分配内核栈时，如果该函数被中断打断，
// 而切换的任务使用dealloc_from_kernel_space回收内核栈，对
// KernelMapper的可变引用获取将会失败造成错误
static KSTACK_LOCK: SpinLock<()> = SpinLock::new(());

unsafe fn alloc_from_kernel_space() -> (VirtAddr, PhysAddr) {
    use crate::arch::MMArch;
    use crate::mm::allocator::page_frame::{allocate_page_frames, PageFrameCount};
    use crate::mm::kernel_mapper::KernelMapper;
    use crate::mm::page::EntryFlags;
    use crate::mm::MemoryManagementArch;

    // Layout
    // ---------------
    // | KernelStack |
    // | guard page  | size == KernelStack::SIZE
    // | KernelStack |
    // | guard page  |
    // | ..........  |
    // ---------------

    let _guard = KSTACK_LOCK.try_lock_irqsave().unwrap();
    let need_size = KernelStack::SIZE * 2;
    let page_num = PageFrameCount::new(need_size.div_ceil(MMArch::PAGE_SIZE).next_power_of_two());

    let (paddr, _count) = allocate_page_frames(page_num).expect("kernel stack alloc failed");

    let guard_vaddr = MMArch::phys_2_virt(paddr).unwrap();
    let _kstack_paddr = paddr + KernelStack::SIZE;
    let kstack_vaddr = guard_vaddr + KernelStack::SIZE;

    core::ptr::write_bytes(kstack_vaddr.data() as *mut u8, 0, KernelStack::SIZE);

    let guard_flags = EntryFlags::new();

    let mut kernel_mapper = KernelMapper::lock();
    let kernel_mapper = kernel_mapper.as_mut().unwrap();

    for i in 0..KernelStack::SIZE / MMArch::PAGE_SIZE {
        let guard_page_vaddr = guard_vaddr + i * MMArch::PAGE_SIZE;
        // Map the guard page
        let flusher = kernel_mapper.remap(guard_page_vaddr, guard_flags).unwrap();
        flusher.flush();
    }

    // unsafe {
    //     log::debug!(
    //         "trigger kernel stack guard page :{:#x}",
    //         (kstack_vaddr.data() - 8)
    //     );
    //     let guard_ptr = (kstack_vaddr.data() - 8) as *mut usize;
    //     guard_ptr.write(0xfff); // Invalid
    // }

    // log::info!(
    //     "[kernel stack alloc]: virt: {:#x}, phy: {:#x}",
    //     kstack_vaddr.data(),
    //     _kstack_paddr.data()
    // );
    (guard_vaddr, paddr)
}

unsafe fn dealloc_from_kernel_space(vaddr: VirtAddr, paddr: PhysAddr) {
    use crate::arch::mm::kernel_page_flags;
    use crate::arch::MMArch;
    use crate::mm::allocator::page_frame::{deallocate_page_frames, PageFrameCount, PhysPageFrame};
    use crate::mm::kernel_mapper::KernelMapper;
    use crate::mm::MemoryManagementArch;

    let _guard = KSTACK_LOCK.try_lock_irqsave().unwrap();

    let need_size = KernelStack::SIZE * 2;
    let page_num = PageFrameCount::new(need_size.div_ceil(MMArch::PAGE_SIZE).next_power_of_two());

    // log::info!(
    //     "[kernel stack dealloc]: virt: {:#x}, phy: {:#x}",
    //     vaddr.data(),
    //     paddr.data()
    // );

    let mut kernel_mapper = KernelMapper::lock();
    let kernel_mapper = kernel_mapper.as_mut().unwrap();

    // restore the guard page flags
    for i in 0..KernelStack::SIZE / MMArch::PAGE_SIZE {
        let guard_page_vaddr = vaddr + i * MMArch::PAGE_SIZE;
        let flusher = kernel_mapper
            .remap(guard_page_vaddr, kernel_page_flags(vaddr))
            .unwrap();
        flusher.flush();
    }

    // release the physical page
    unsafe { deallocate_page_frames(PhysPageFrame::new(paddr), page_num) };
}

impl KernelStack {
    pub const SIZE: usize = 0x8000;
    pub const ALIGN: usize = 0x8000;

    pub fn new() -> Result<Self, SystemError> {
        if cfg!(feature = "kstack_protect") {
            unsafe {
                let (kstack_vaddr, kstack_paddr) = alloc_from_kernel_space();
                let real_kstack_vaddr = kstack_vaddr + KernelStack::SIZE;
                Ok(Self {
                    stack: Some(
                        AlignedBox::<[u8; KernelStack::SIZE], { KernelStack::ALIGN }>::new_unchecked(
                            real_kstack_vaddr.data() as *mut [u8; KernelStack::SIZE],
                        ),
                    ),
                    ty: KernelStackType::KernelSpace(kstack_vaddr, kstack_paddr),
                })
            }
        } else {
            Ok(Self {
                stack: Some(
                    AlignedBox::<[u8; KernelStack::SIZE], { KernelStack::ALIGN }>::new_zeroed()?,
                ),
                ty: KernelStackType::Dynamic,
            })
        }
    }

    /// 根据已有的空间，构造一个内核栈结构体
    ///
    /// 仅仅用于BSP启动时，为idle进程构造内核栈。其他时候使用这个函数，很可能造成错误！
    pub unsafe fn from_existed(base: VirtAddr) -> Result<Self, SystemError> {
        if base.is_null() || !base.check_aligned(Self::ALIGN) {
            return Err(SystemError::EFAULT);
        }

        Ok(Self {
            stack: Some(
                AlignedBox::<[u8; KernelStack::SIZE], { KernelStack::ALIGN }>::new_unchecked(
                    base.data() as *mut [u8; KernelStack::SIZE],
                ),
            ),
            ty: KernelStackType::Static,
        })
    }

    pub fn guard_page_address(&self) -> Option<VirtAddr> {
        match self.ty {
            KernelStackType::KernelSpace(kstack_virt_addr, _) => {
                return Some(kstack_virt_addr);
            }
            _ => {
                // 静态内核栈和动态内核栈没有guard page
                return None;
            }
        }
    }

    pub fn guard_page_size(&self) -> Option<usize> {
        match self.ty {
            KernelStackType::KernelSpace(_, _) => {
                return Some(KernelStack::SIZE);
            }
            _ => {
                // 静态内核栈和动态内核栈没有guard page
                return None;
            }
        }
    }

    /// 返回内核栈的起始虚拟地址(低地址)
    pub fn start_address(&self) -> VirtAddr {
        return VirtAddr::new(self.stack.as_ref().unwrap().as_ptr() as usize);
    }

    /// 返回内核栈的结束虚拟地址(高地址)(不包含该地址)
    pub fn stack_max_address(&self) -> VirtAddr {
        return VirtAddr::new(self.stack.as_ref().unwrap().as_ptr() as usize + Self::SIZE);
    }

    pub unsafe fn set_pcb(&mut self, pcb: Weak<ProcessControlBlock>) -> Result<(), SystemError> {
        // 将一个Weak<ProcessControlBlock>放到内核栈的最低地址处
        let p: *const ProcessControlBlock = Weak::into_raw(pcb);
        let stack_bottom_ptr = self.start_address().data() as *mut *const ProcessControlBlock;

        // 如果内核栈的最低地址处已经有了一个pcb，那么，这里就不再设置,直接返回错误
        if unlikely(unsafe { !(*stack_bottom_ptr).is_null() }) {
            error!("kernel stack bottom is not null: {:p}", *stack_bottom_ptr);
            return Err(SystemError::EPERM);
        }
        // 将pcb的地址放到内核栈的最低地址处
        unsafe {
            *stack_bottom_ptr = p;
        }

        return Ok(());
    }

    /// 清除内核栈的pcb指针
    ///
    /// ## 参数
    ///
    /// - `force` : 如果为true,那么，即使该内核栈的pcb指针不为null，也会被强制清除而不处理Weak指针问题
    pub unsafe fn clear_pcb(&mut self, force: bool) {
        let stack_bottom_ptr = self.start_address().data() as *mut *const ProcessControlBlock;
        if unlikely(unsafe { (*stack_bottom_ptr).is_null() }) {
            return;
        }

        if !force {
            let pcb_ptr: Weak<ProcessControlBlock> = Weak::from_raw(*stack_bottom_ptr);
            drop(pcb_ptr);
        }

        *stack_bottom_ptr = core::ptr::null();
    }

    /// 返回指向当前内核栈pcb的Arc指针
    #[allow(dead_code)]
    pub unsafe fn pcb(&self) -> Option<Arc<ProcessControlBlock>> {
        // 从内核栈的最低地址处取出pcb的地址
        let p = self.stack.as_ref().unwrap().as_ptr() as *const *const ProcessControlBlock;
        if unlikely(unsafe { (*p).is_null() }) {
            return None;
        }

        // 为了防止内核栈的pcb指针被释放，这里需要将其包装一下，使得Arc的drop不会被调用
        let weak_wrapper: ManuallyDrop<Weak<ProcessControlBlock>> =
            ManuallyDrop::new(Weak::from_raw(*p));

        let new_arc: Arc<ProcessControlBlock> = weak_wrapper.upgrade()?;
        return Some(new_arc);
    }
}

impl Drop for KernelStack {
    fn drop(&mut self) {
        if self.stack.is_some() {
            let ptr = self.stack.as_ref().unwrap().as_ptr() as *const *const ProcessControlBlock;
            if unsafe { !(*ptr).is_null() } {
                let pcb_ptr: Weak<ProcessControlBlock> = unsafe { Weak::from_raw(*ptr) };
                drop(pcb_ptr);
            }
        }
        match self.ty {
            KernelStackType::KernelSpace(kstack_virt_addr, kstack_phy_addr) => {
                // 释放内核栈
                unsafe {
                    dealloc_from_kernel_space(kstack_virt_addr, kstack_phy_addr);
                }
                let bx = self.stack.take();
                core::mem::forget(bx);
            }
            KernelStackType::Static => {
                let bx = self.stack.take();
                core::mem::forget(bx);
            }
            KernelStackType::Dynamic => {}
        }
    }
}

pub fn process_init() {
    ProcessManager::init();
}

#[derive(Debug)]
pub struct ProcessSignalInfo {
    // 当前进程被屏蔽的信号
    sig_blocked: SigSet,
    // 暂存旧信号，用于恢复
    saved_sigmask: SigSet,
    // sig_pending 中存储当前线程要处理的信号
    sig_pending: SigPending,

    // 当前进程对应的tty
    tty: Option<Arc<TtyCore>>,
    has_child_subreaper: bool,

    /// 标记当前进程是否是一个“子进程收割者”
    ///
    /// todo: 在prctl里面实现设置这个标志位的功能
    is_child_subreaper: bool,

    /// boolean value for session group leader
    pub is_session_leader: bool,
}

impl ProcessSignalInfo {
    pub fn sig_blocked(&self) -> &SigSet {
        &self.sig_blocked
    }

    pub fn sig_pending(&self) -> &SigPending {
        &self.sig_pending
    }

    pub fn sig_pending_mut(&mut self) -> &mut SigPending {
        &mut self.sig_pending
    }

    pub fn sig_block_mut(&mut self) -> &mut SigSet {
        &mut self.sig_blocked
    }

    pub fn saved_sigmask(&self) -> &SigSet {
        &self.saved_sigmask
    }

    pub fn saved_sigmask_mut(&mut self) -> &mut SigSet {
        &mut self.saved_sigmask
    }

    pub fn tty(&self) -> Option<Arc<TtyCore>> {
        self.tty.clone()
    }

    pub fn set_tty(&mut self, tty: Option<Arc<TtyCore>>) {
        self.tty = tty;
    }

    /// 从 pcb 的 siginfo中取出下一个要处理的信号，先处理线程信号，再处理进程信号
    ///
    /// ## 参数
    ///
    /// - `sig_mask` 被忽略掉的信号
    ///
    pub fn dequeue_signal(
        &mut self,
        sig_mask: &SigSet,
        pcb: &Arc<ProcessControlBlock>,
    ) -> (Signal, Option<SigInfo>) {
        let res = self.sig_pending.dequeue_signal(sig_mask);
        pcb.recalc_sigpending(Some(self));
        if res.0 != Signal::INVALID {
            return res;
        } else {
            let sighand = pcb.sighand();
            let res = sighand.shared_pending_dequeue(sig_mask);
            pcb.recalc_sigpending(Some(self));
            return res;
        }
    }

    pub fn has_child_subreaper(&self) -> bool {
        self.has_child_subreaper
    }

    pub fn set_has_child_subreaper(&mut self, has_child_subreaper: bool) {
        self.has_child_subreaper = has_child_subreaper;
    }

    pub fn is_child_subreaper(&self) -> bool {
        self.is_child_subreaper
    }

    pub fn set_is_child_subreaper(&mut self, is_child_subreaper: bool) {
        self.is_child_subreaper = is_child_subreaper;
    }
}

impl Default for ProcessSignalInfo {
    fn default() -> Self {
        Self {
            sig_blocked: SigSet::empty(),
            saved_sigmask: SigSet::empty(),
            sig_pending: SigPending::default(),
            tty: None,
            has_child_subreaper: false,
            is_child_subreaper: false,
            is_session_leader: false,
        }
    }
}
