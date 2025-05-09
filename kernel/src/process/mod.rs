use core::{
    fmt,
    hash::Hash,
    hint::spin_loop,
    intrinsics::{likely, unlikely},
    mem::ManuallyDrop,
    sync::atomic::{compiler_fence, fence, AtomicBool, AtomicUsize, Ordering},
};

use alloc::{
    ffi::CString,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use cred::INIT_CRED;
use hashbrown::HashMap;
use log::{debug, error, info, warn};
use process_group::{Pgid, ProcessGroup, ALL_PROCESS_GROUP};
use session::{Session, Sid, ALL_SESSION};
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
        procfs::procfs_unregister_pid,
        vfs::{file::FileDescriptorVec, FileType, IndexNode},
    },
    ipc::{
        signal::RestartBlock,
        signal_types::{SigInfo, SigPending, SignalStruct},
    },
    libs::{
        align::AlignedBox,
        casting::DowncastArc,
        futex::{
            constant::{FutexFlag, FUTEX_BITSET_MATCH_ANY},
            futex::{Futex, RobustListHead},
        },
        lock_free_flags::LockFreeFlags,
        mutex::Mutex,
        rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard},
        spinlock::{SpinLock, SpinLockGuard},
        wait_queue::WaitQueue,
    },
    mm::{
        percpu::{PerCpu, PerCpuVar},
        set_IDLE_PROCESS_ADDRESS_SPACE,
        ucontext::AddressSpace,
        VirtAddr,
    },
    namespaces::{mnt_namespace::FsStruct, pid_namespace::PidStrcut, NsProxy},
    net::socket::SocketInode,
    sched::{
        completion::Completion, cpu_rq, fair::FairSchedEntity, prio::MAX_PRIO, DequeueFlag,
        EnqueueFlag, OnRq, SchedMode, WakeupFlags, __schedule,
    },
    smp::{
        core::smp_get_processor_id,
        cpu::{AtomicProcessorId, ProcessorId},
        kick_cpu,
    },
    syscall::{user_access::clear_user, Syscall},
};
use timer::AlarmTimer;

use self::{cred::Cred, kthread::WorkerPrivate};

pub mod abi;
pub mod cred;
pub mod exec;
pub mod exit;
pub mod fork;
pub mod idle;
pub mod kthread;
pub mod pid;
pub mod process_group;
pub mod resource;
pub mod session;
pub mod stdio;
pub mod syscall;
pub mod timer;
pub mod utils;

/// 系统中所有进程的pcb
static ALL_PROCESS: SpinLock<Option<HashMap<Pid, Arc<ProcessControlBlock>>>> = SpinLock::new(None);

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
        ALL_PROCESS_GROUP.lock_irqsave().replace(HashMap::new());
        ALL_SESSION.lock_irqsave().replace(HashMap::new());
        Self::init_switch_result();
        Self::arch_init();
        debug!("process arch init done.");
        Self::init_idle();
        debug!("process idle init done.");

        unsafe { __PROCESS_MANAGEMENT_INIT_DONE = true };
        info!("Process Manager initialized.");
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
    pub fn current_pid() -> Pid {
        if unlikely(unsafe { !__PROCESS_MANAGEMENT_INIT_DONE }) {
            return Pid(0);
        }

        return ProcessManager::current_pcb().pid();
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
    pub fn find(pid: Pid) -> Option<Arc<ProcessControlBlock>> {
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
            .insert(pcb.pid(), pcb.clone());
    }

    /// ### 获取所有进程的pid
    pub fn get_all_processes() -> Vec<Pid> {
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
    fn exit_notify() {
        let current = ProcessManager::current_pcb();
        // 让INIT进程收养所有子进程
        if current.pid() != Pid(1) {
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
            let r = Syscall::kill_process(parent_pcb.pid(), Signal::SIGCHLD);
            if r.is_err() {
                warn!(
                    "failed to send kill signal to {:?}'s parent pcb {:?}",
                    current.pid(),
                    parent_pcb.pid()
                );
            }
            // todo: 这里需要向父进程发送SIGCHLD信号
            // todo: 这里还需要根据线程组的信息，决定信号的发送
        }
    }

    /// 退出当前进程
    ///
    /// ## 参数
    ///
    /// - `exit_code` : 进程的退出码
    pub fn exit(exit_code: usize) -> ! {
        // 关中断
        let _irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        let pid: Pid;
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
            if let Some(addr) = thread.set_child_tid {
                unsafe { clear_user(addr, core::mem::size_of::<i32>()).expect("clear tid failed") };
            }

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

            pcb.clear_pg_and_session_reference();
            drop(pcb);
            ProcessManager::exit_notify();
        }

        __schedule(SchedMode::SM_NONE);
        error!("pid {pid:?} exited but sched again!");
        #[allow(clippy::empty_loop)]
        loop {
            spin_loop();
        }
    }

    pub unsafe fn release(pid: Pid) {
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
            if pcb.pid() == cpu_rq(cpu_id.data() as usize).current().pid() {
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

int_like!(Pid, AtomicPid, usize, AtomicUsize);

impl fmt::Display for Pid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
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
    pid: Pid,
    /// 当前进程的线程组id（这个值在同一个线程组内永远不变）
    tgid: Pid,
    /// 有关Pid的相关的信息
    thread_pid: Arc<RwLock<PidStrcut>>,
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
    /// 信号处理结构体
    sig_struct: SpinLock<SignalStruct>,
    /// 退出信号S
    exit_signal: AtomicSignal,

    /// 父进程指针
    parent_pcb: RwLock<Weak<ProcessControlBlock>>,
    /// 真实父进程指针
    real_parent_pcb: RwLock<Weak<ProcessControlBlock>>,

    /// 子进程链表
    children: RwLock<Vec<Pid>>,

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

    /// namespace的指针
    nsproxy: Arc<RwLock<NsProxy>>,

    /// 进程作为主体的凭证集
    cred: SpinLock<Cred>,
    self_ref: Weak<ProcessControlBlock>,

    restart_block: SpinLock<Option<RestartBlock>>,

    /// 进程组
    process_group: Mutex<Weak<ProcessGroup>>,

    /// 进程的可执行文件路径
    executable_path: RwLock<String>,
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
        let (pid, ppid, cwd, cred, tty) = if is_idle {
            let cred = INIT_CRED.clone();
            (Pid(0), Pid(0), "/".to_string(), cred, None)
        } else {
            let ppid = ProcessManager::current_pcb().pid();
            let mut cred = ProcessManager::current_pcb().cred();
            cred.cap_permitted = cred.cap_ambient;
            cred.cap_effective = cred.cap_ambient;
            let cwd = ProcessManager::current_pcb().basic().cwd();
            let tty = ProcessManager::current_pcb().sig_info_irqsave().tty();
            (Self::generate_pid(), ppid, cwd, cred, tty)
        };

        let basic_info = ProcessBasicInfo::new(ppid, name.clone(), cwd, None);
        let preempt_count = AtomicUsize::new(0);
        let flags = unsafe { LockFreeFlags::new(ProcessFlags::empty()) };

        let sched_info = ProcessSchedulerInfo::new(None);
        let arch_info = SpinLock::new(ArchPCBInfo::new(&kstack));

        let ppcb: Weak<ProcessControlBlock> = ProcessManager::find(ppid)
            .map(|p| Arc::downgrade(&p))
            .unwrap_or_default();
        let mut pcb = Self {
            pid,
            tgid: pid,
            thread_pid: Arc::new(RwLock::new(PidStrcut::new())),
            basic: basic_info,
            preempt_count,
            flags,
            kernel_stack: RwLock::new(kstack),
            syscall_stack: RwLock::new(KernelStack::new().unwrap()),
            worker_private: SpinLock::new(None),
            sched_info,
            arch_info,
            sig_info: RwLock::new(ProcessSignalInfo::default()),
            sig_struct: SpinLock::new(SignalStruct::new()),
            exit_signal: AtomicSignal::new(Signal::SIGCHLD),
            parent_pcb: RwLock::new(ppcb.clone()),
            real_parent_pcb: RwLock::new(ppcb),
            children: RwLock::new(Vec::new()),
            wait_queue: WaitQueue::default(),
            thread: RwLock::new(ThreadInfo::new()),
            fs: RwLock::new(Arc::new(FsStruct::new())),
            alarm_timer: SpinLock::new(None),
            robust_list: RwLock::new(None),
            nsproxy: Arc::new(RwLock::new(NsProxy::new())),
            cred: SpinLock::new(cred),
            self_ref: Weak::new(),
            restart_block: SpinLock::new(None),
            process_group: Mutex::new(Weak::new()),
            executable_path: RwLock::new(name),
        };

        pcb.sig_info.write().set_tty(tty);

        // 初始化系统调用栈
        #[cfg(target_arch = "x86_64")]
        pcb.arch_info
            .lock()
            .init_syscall_stack(&pcb.syscall_stack.read());

        let pcb = Arc::new_cyclic(|weak| {
            pcb.self_ref = weak.clone();
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

        // 将当前pcb加入父进程的子进程哈希表中
        if pcb.pid() > Pid(1) {
            if let Some(ppcb_arc) = pcb.parent_pcb.read_irqsave().upgrade() {
                let mut children = ppcb_arc.children.write_irqsave();
                children.push(pcb.pid());
            } else {
                panic!("parent pcb is None");
            }
        }

        if pcb.pid() > Pid(0) && !is_idle {
            let process_group = ProcessGroup::new(pcb.clone());
            *pcb.process_group.lock() = Arc::downgrade(&process_group);
            ProcessManager::add_process_group(process_group.clone());

            let session = Session::new(process_group.clone());
            process_group.process_group_inner.lock().session = Arc::downgrade(&session);
            session.session_inner.lock().leader = Some(pcb.clone());
            ProcessManager::add_session(session);

            ProcessManager::add_pcb(pcb.clone());
        }
        // log::debug!(
        //     "A new process is created, pid: {:?}, pgid: {:?}, sid: {:?}",
        //     pcb.pid(),
        //     pcb.process_group().unwrap().pgid(),
        //     pcb.session().unwrap().sid()
        // );

        return pcb;
    }

    /// 生成一个新的pid
    #[inline(always)]
    fn generate_pid() -> Pid {
        static NEXT_PID: AtomicPid = AtomicPid::new(Pid(1));
        return NEXT_PID.fetch_add(Pid(1), Ordering::SeqCst);
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
    pub fn contain_child(&self, pid: &Pid) -> bool {
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
    pub fn basic(&self) -> RwLockReadGuard<ProcessBasicInfo> {
        return self.basic.read_irqsave();
    }

    #[inline(always)]
    pub fn set_name(&self, name: String) {
        self.basic.write().set_name(name);
    }

    #[inline(always)]
    pub fn basic_mut(&self) -> RwLockWriteGuard<ProcessBasicInfo> {
        return self.basic.write_irqsave();
    }

    /// # 获取arch info的锁，同时关闭中断
    #[inline(always)]
    pub fn arch_info_irqsave(&self) -> SpinLockGuard<ArchPCBInfo> {
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
    pub unsafe fn arch_info(&self) -> SpinLockGuard<ArchPCBInfo> {
        return self.arch_info.lock();
    }

    #[inline(always)]
    pub fn kernel_stack(&self) -> RwLockReadGuard<KernelStack> {
        return self.kernel_stack.read();
    }

    pub unsafe fn kernel_stack_force_ref(&self) -> &KernelStack {
        self.kernel_stack.force_get_ref()
    }

    #[inline(always)]
    #[allow(dead_code)]
    pub fn kernel_stack_mut(&self) -> RwLockWriteGuard<KernelStack> {
        return self.kernel_stack.write();
    }

    #[inline(always)]
    pub fn sched_info(&self) -> &ProcessSchedulerInfo {
        return &self.sched_info;
    }

    #[inline(always)]
    pub fn worker_private(&self) -> SpinLockGuard<Option<WorkerPrivate>> {
        return self.worker_private.lock();
    }

    #[inline(always)]
    pub fn pid(&self) -> Pid {
        return self.pid;
    }

    #[inline(always)]
    pub fn pid_strcut(&self) -> Arc<RwLock<PidStrcut>> {
        self.thread_pid.clone()
    }

    #[inline(always)]
    pub fn tgid(&self) -> Pid {
        return self.tgid;
    }

    #[inline(always)]
    pub fn fs_struct(&self) -> Arc<FsStruct> {
        self.fs.read().clone()
    }

    pub fn fs_struct_mut(&self) -> RwLockWriteGuard<Arc<FsStruct>> {
        self.fs.write()
    }

    pub fn pwd_inode(&self) -> Arc<dyn IndexNode> {
        self.fs.read().pwd()
    }

    /// 获取文件描述符表的Arc指针
    #[inline(always)]
    pub fn fd_table(&self) -> Arc<RwLock<FileDescriptorVec>> {
        return self.basic.read().fd_table().unwrap();
    }

    #[inline(always)]
    pub fn cred(&self) -> Cred {
        self.cred.lock().clone()
    }

    pub fn set_execute_path(&self, path: String) {
        *self.executable_path.write() = path;
    }

    pub fn execute_path(&self) -> String {
        self.executable_path.read().clone()
    }

    /// 根据文件描述符序号，获取socket对象的Arc指针
    ///
    /// ## 参数
    ///
    /// - `fd` 文件描述符序号
    ///
    /// ## 返回值
    ///
    /// Option(&mut Box<dyn Socket>) socket对象的可变引用. 如果文件描述符不是socket，那么返回None
    pub fn get_socket(&self, fd: i32) -> Option<Arc<SocketInode>> {
        let binding = ProcessManager::current_pcb().fd_table();
        let fd_table_guard = binding.read();

        let f = fd_table_guard.get_file_by_fd(fd)?;
        drop(fd_table_guard);

        if f.file_type() != FileType::Socket {
            return None;
        }
        let socket: Arc<SocketInode> = f
            .inode()
            .downcast_arc::<SocketInode>()
            .expect("Not a socket inode");
        return Some(socket);
    }

    /// 当前进程退出时,让初始进程收养所有子进程
    unsafe fn adopt_childen(&self) -> Result<(), SystemError> {
        match ProcessManager::find(Pid(1)) {
            Some(init_pcb) => {
                let childen_guard = self.children.write();
                let mut init_childen_guard = init_pcb.children.write();

                childen_guard.iter().for_each(|pid| {
                    init_childen_guard.push(*pid);
                });

                return Ok(());
            }
            _ => Err(SystemError::ECHILD),
        }
    }

    /// 生成进程的名字
    pub fn generate_name(program_path: &str, args: &Vec<CString>) -> String {
        let mut name = program_path.to_string();
        for arg in args {
            name.push(' ');
            name.push_str(arg.to_string_lossy().as_ref());
        }
        return name;
    }

    pub fn sig_info_irqsave(&self) -> RwLockReadGuard<ProcessSignalInfo> {
        self.sig_info.read_irqsave()
    }

    pub fn try_siginfo_irqsave(&self, times: u8) -> Option<RwLockReadGuard<ProcessSignalInfo>> {
        for _ in 0..times {
            if let Some(r) = self.sig_info.try_read_irqsave() {
                return Some(r);
            }
        }

        return None;
    }

    pub fn sig_info_mut(&self) -> RwLockWriteGuard<ProcessSignalInfo> {
        self.sig_info.write_irqsave()
    }

    pub fn try_siginfo_mut(&self, times: u8) -> Option<RwLockWriteGuard<ProcessSignalInfo>> {
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
        let has_pending = sig_info.sig_pending().has_pending();
        drop(sig_info);
        return has_pending;
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

    pub fn sig_struct(&self) -> SpinLockGuard<SignalStruct> {
        self.sig_struct.lock_irqsave()
    }

    pub fn try_sig_struct_irqsave(&self, times: u8) -> Option<SpinLockGuard<SignalStruct>> {
        for _ in 0..times {
            if let Ok(r) = self.sig_struct.try_lock_irqsave() {
                return Some(r);
            }
        }

        return None;
    }

    pub fn sig_struct_irqsave(&self) -> SpinLockGuard<SignalStruct> {
        self.sig_struct.lock_irqsave()
    }

    #[inline(always)]
    pub fn get_robust_list(&self) -> RwLockReadGuard<Option<RobustListHead>> {
        return self.robust_list.read_irqsave();
    }

    #[inline(always)]
    pub fn set_robust_list(&self, new_robust_list: Option<RobustListHead>) {
        *self.robust_list.write_irqsave() = new_robust_list;
    }

    pub fn alarm_timer_irqsave(&self) -> SpinLockGuard<Option<AlarmTimer>> {
        return self.alarm_timer.lock_irqsave();
    }

    pub fn get_nsproxy(&self) -> Arc<RwLock<NsProxy>> {
        self.nsproxy.clone()
    }

    pub fn set_nsproxy(&self, nsprsy: NsProxy) {
        *self.nsproxy.write() = nsprsy;
    }

    /// Exit fd table when process exit
    fn exit_files(&self) {
        self.basic.write_irqsave().set_fd_table(None);
    }

    pub fn children_read_irqsave(&self) -> RwLockReadGuard<Vec<Pid>> {
        self.children.read_irqsave()
    }

    pub fn threads_read_irqsave(&self) -> RwLockReadGuard<ThreadInfo> {
        self.thread.read_irqsave()
    }

    pub fn restart_block(&self) -> SpinLockGuard<Option<RestartBlock>> {
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
}

impl Drop for ProcessControlBlock {
    fn drop(&mut self) {
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        // 在ProcFS中,解除进程的注册
        procfs_unregister_pid(self.pid())
            .unwrap_or_else(|e| panic!("procfs_unregister_pid failed: error: {e:?}"));

        if let Some(ppcb) = self.parent_pcb.read_irqsave().upgrade() {
            ppcb.children
                .write_irqsave()
                .retain(|pid| *pid != self.pid());
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
        }
    }

    pub fn group_leader(&self) -> Option<Arc<ProcessControlBlock>> {
        return self.group_leader.upgrade();
    }
}

/// 进程的基本信息
///
/// 这个结构体保存进程的基本信息，主要是那些不会随着进程的运行而经常改变的信息。
#[derive(Debug)]
pub struct ProcessBasicInfo {
    /// 当前进程的父进程的pid
    ppid: Pid,
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
        ppid: Pid,
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

    pub fn ppid(&self) -> Pid {
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

    pub fn fd_table(&self) -> Option<Arc<RwLock<FileDescriptorVec>>> {
        return self.fd_table.clone();
    }

    pub fn set_fd_table(&mut self, fd_table: Option<Arc<RwLock<FileDescriptorVec>>>) {
        self.fd_table = fd_table;
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

    pub fn inner_lock_write_irqsave(&self) -> RwLockWriteGuard<InnerSchedInfo> {
        return self.inner_locked.write_irqsave();
    }

    pub fn inner_lock_read_irqsave(&self) -> RwLockReadGuard<InnerSchedInfo> {
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

#[derive(Debug, Clone)]
pub struct KernelStack {
    stack: Option<AlignedBox<[u8; KernelStack::SIZE], { KernelStack::ALIGN }>>,
    /// 标记该内核栈是否可以被释放
    can_be_freed: bool,
}

impl KernelStack {
    pub const SIZE: usize = 0x4000;
    pub const ALIGN: usize = 0x4000;

    pub fn new() -> Result<Self, SystemError> {
        return Ok(Self {
            stack: Some(
                AlignedBox::<[u8; KernelStack::SIZE], { KernelStack::ALIGN }>::new_zeroed()?,
            ),
            can_be_freed: true,
        });
    }

    /// 根据已有的空间，构造一个内核栈结构体
    ///
    /// 仅仅用于BSP启动时，为idle进程构造内核栈。其他时候使用这个函数，很可能造成错误！
    pub unsafe fn from_existed(base: VirtAddr) -> Result<Self, SystemError> {
        if base.is_null() || !base.check_aligned(Self::ALIGN) {
            return Err(SystemError::EFAULT);
        }

        return Ok(Self {
            stack: Some(
                AlignedBox::<[u8; KernelStack::SIZE], { KernelStack::ALIGN }>::new_unchecked(
                    base.data() as *mut [u8; KernelStack::SIZE],
                ),
            ),
            can_be_freed: false,
        });
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
        // 如果该内核栈不可以被释放，那么，这里就forget，不调用AlignedBox的drop函数
        if !self.can_be_freed {
            let bx = self.stack.take();
            core::mem::forget(bx);
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
    // sig_shared_pending 中存储当前线程所属进程要处理的信号
    sig_shared_pending: SigPending,
    // 当前进程对应的tty
    tty: Option<Arc<TtyCore>>,
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

    pub fn sig_shared_pending_mut(&mut self) -> &mut SigPending {
        &mut self.sig_shared_pending
    }

    pub fn sig_shared_pending(&self) -> &SigPending {
        &self.sig_shared_pending
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
            let res = self.sig_shared_pending.dequeue_signal(sig_mask);
            pcb.recalc_sigpending(Some(self));
            return res;
        }
    }
}

impl Default for ProcessSignalInfo {
    fn default() -> Self {
        Self {
            sig_blocked: SigSet::empty(),
            saved_sigmask: SigSet::empty(),
            sig_pending: SigPending::default(),
            sig_shared_pending: SigPending::default(),
            tty: None,
        }
    }
}
