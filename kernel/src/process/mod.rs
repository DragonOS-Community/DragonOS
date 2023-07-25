use core::{
    intrinsics::unlikely,
    mem::ManuallyDrop,
    ptr::null_mut,
    sync::atomic::{compiler_fence, AtomicUsize, Ordering},
};

use alloc::{boxed::Box, string::String, sync::Arc};

use crate::{
    arch::{asm::current::current_pcb, fpu::FpState, process::ArchPCBInfo},
    filesystem::vfs::file::FileDescriptorVec,
    ipc::signal_types::{sighand_struct, signal_struct, sigpending, sigset_t, SignalNumber},
    kdebug,
    libs::{align::AlignedBox, rwlock::{RwLock, RwLockReadGuard}, spinlock::{SpinLock, SpinLockGuard}},
    mm::{
        set_INITIAL_PROCESS_ADDRESS_SPACE, ucontext::AddressSpace, INITIAL_PROCESS_ADDRESS_SPACE,
    },
    sched::SchedPolicy,
    syscall::SystemError,
};

use self::initial_proc::{INITIAL_SIGHAND, INITIAL_SIGNALS};

pub mod abi;
pub mod c_adapter;
pub mod exec;
pub mod fork;
pub mod initial_proc;
pub mod pid;
pub mod preempt;
pub mod process;
pub mod syscall;

int_like!(Pid, AtomicPid, usize, AtomicUsize);

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
    Stopped(SignalNumber),
    /// 进程已经退出，usize表示进程的退出码
    Exited(usize),
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
    }
}

#[derive(Debug)]
pub struct ProcessControlBlock {
    basic: RwLock<ProcessBasicInfo>,
    /// 当前进程的自旋锁持有计数
    preempt_count: AtomicUsize,

    flags: SpinLock<ProcessFlags>,
    signal: RwLock<ProcessSignalInfo>,

    /// 浮点寄存器的状态
    fp_state: SpinLock<Option<FpState>>,

    /// 进程的内核栈
    kernel_stack: RwLock<KernelStack>,

    /// 与调度相关的信息
    sched_info: RwLock<ProcessSchedulerInfo>,
    /// 与处理器架构相关的信息
    arch_info: SpinLock<ArchPCBInfo>,
}

impl ProcessControlBlock {
    pub fn new(name: String, kstack: KernelStack) -> Arc<Self> {
        let basic_info = ProcessBasicInfo::new(Self::generate_pid(), Pid(0), Pid(0), name, None);
        let preempt_count = AtomicUsize::new(0);
        let flags = SpinLock::new(ProcessFlags::empty());
        let signal = ProcessSignalInfo::new();
        let fp_state = SpinLock::new(None);
        let sched_info = ProcessSchedulerInfo::new(None);
        let arch_info = SpinLock::new(ArchPCBInfo::new());

        let pcb = Self {
            basic: basic_info,
            preempt_count,
            flags,
            signal,
            fp_state,
            kernel_stack: RwLock::new(kstack),
            sched_info,
            arch_info,
        };

        let pcb = Arc::new(pcb);

        unsafe { pcb.kernel_stack.write().set_pcb(Arc::clone(&pcb)).unwrap() };

        return pcb;
    }

    /// 获取当前进程的pcb
    pub fn current_pcb() -> Arc<Self>{
        return Self::arch_current_pcb();
    }

    /// 生成一个新的pid
    fn generate_pid() -> Pid {
        static NEXT_PID: AtomicPid = AtomicPid::new(Pid(1));
        return NEXT_PID.fetch_add(Pid(1), Ordering::SeqCst);
    }

    /// 返回当前进程的锁持有计数
    pub fn preempt_count(&self) -> usize {
        return self.preempt_count.load(Ordering::SeqCst);
    }
    
    /// 增加当前进程的锁持有计数
    pub fn preempt_disable(&self) {
        self.preempt_count.fetch_add(1, Ordering::SeqCst);
    }

    /// 减少当前进程的锁持有计数
    pub fn preempt_enable(&self) {
        self.preempt_count.fetch_sub(1, Ordering::SeqCst);
    }

    pub fn flags(&self) -> SpinLockGuard<ProcessFlags>{
        return self.flags.lock();
    }

    pub fn basic(&self) -> RwLockReadGuard<ProcessBasicInfo>{
        return self.basic.read();
    }

    pub fn set_name(&self, name: String) {
        self.basic.write().set_name(name);
    }
    

}

/// 进程的基本信息
///
/// 这个结构体保存进程的基本信息，主要是那些不会随着进程的运行而经常改变的信息。
#[derive(Debug)]
pub struct ProcessBasicInfo {
    /// 当前进程的pid
    pid: Pid,
    /// 当前进程的进程组id
    pgid: Pid,
    /// 当前进程的父进程的pid
    ppid: Pid,
    /// 进程的名字
    name: String,

    /// 用户地址空间
    user_vm: Option<Arc<AddressSpace>>,

    /// 文件描述符表
    fd_table: Option<Arc<RwLock<FileDescriptorVec>>>,
}

impl ProcessBasicInfo {
    pub fn new(
        pid: Pid,
        pgid: Pid,
        ppid: Pid,
        name: String,
        user_vm: Option<Arc<AddressSpace>>,
    ) -> RwLock<Self> {
        return RwLock::new(Self {
            pid,
            pgid,
            ppid,
            name,
            user_vm,
            fd_table: None,
        });
    }

    pub fn pid(&self) -> Pid {
        return self.pid;
    }

    pub fn pgid(&self) -> Pid {
        return self.pgid;
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

    pub fn user_vm(&self) -> Option<Arc<AddressSpace>> {
        return self.user_vm.clone();
    }

    pub fn set_user_vm(&mut self, user_vm: Option<Arc<AddressSpace>>) {
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
    on_cpu: Option<u32>,
    /// 如果当前进程等待被迁移到另一个cpu核心上（也就是flags中的PF_NEED_MIGRATE被置位），
    /// 该字段存储要被迁移到的目标处理器核心号
    migrate_to: Option<u32>,

    /// 当前进程的状态
    state: ProcessState,
    /// 进程的调度策略
    sched_policy: SchedPolicy,
    /// 当前进程的虚拟运行时间
    virtual_runtime: isize,
    /// 由实时调度器管理的时间片
    rt_time_slice: isize,
}

impl ProcessSchedulerInfo {
    pub fn new(on_cpu: Option<u32>) -> RwLock<Self> {
        return RwLock::new(Self {
            on_cpu,
            migrate_to: None,
            state: ProcessState::Blocked(false),
            sched_policy: SchedPolicy::CFS,
            virtual_runtime: 0,
            rt_time_slice: 0,
        });
    }

    pub fn on_cpu(&self) -> Option<u32> {
        return self.on_cpu;
    }

    pub fn set_on_cpu(&mut self, on_cpu: Option<u32>) {
        self.on_cpu = on_cpu;
    }

    pub fn migrate_to(&self) -> Option<u32> {
        return self.migrate_to;
    }

    pub fn set_migrate_to(&mut self, migrate_to: Option<u32>) {
        self.migrate_to = migrate_to;
    }

    pub fn state(&self) -> ProcessState {
        return self.state;
    }

    fn set_state(&mut self, state: ProcessState) {
        self.state = state;
    }
}

#[derive(Debug)]
pub struct ProcessSignalInfo {
    // 信号相关的字段。由于信号机制实现的不是很好，这里写的真的非常丑陋。
    // TODO：重构信号机制，并重写这里的代码
    pub signal: signal_struct,
    pub sighand: sighand_struct,
    pub sig_blocked: sigset_t,
    pub sig_pending: sigpending,
}

impl ProcessSignalInfo {
    pub fn new() -> RwLock<Self> {
        unsafe {
            return RwLock::new(Self {
                signal: INITIAL_SIGNALS.clone(),
                sighand: INITIAL_SIGHAND.clone(),
                sig_blocked: 0,
                sig_pending: sigpending::new(),
            });
        }
    }
}

#[derive(Debug)]
pub struct KernelStack {
    stack: AlignedBox<[u8; KernelStack::SIZE], { KernelStack::ALIGN }>,
}

impl KernelStack {
    pub const SIZE: usize = 0x4000;
    pub const ALIGN: usize = 0x4000;

    pub fn new() -> Result<Self, SystemError> {
        return Ok(Self {
            stack: AlignedBox::<[u8; KernelStack::SIZE], { KernelStack::ALIGN }>::new_zeroed()?,
        });
    }

    /// 返回内核栈的起始虚拟地址(低地址)
    pub fn start_address(&self) -> usize {
        self.stack.as_ptr() as usize
    }

    pub unsafe fn set_pcb(&mut self, pcb: Arc<ProcessControlBlock>) -> Result<(), SystemError> {
        // 将一个Arc<ProcessControlBlock>放到内核栈的最低地址处
        let p: *const ProcessControlBlock = Arc::into_raw(pcb);
        // 如果内核栈的最低地址处已经有了一个pcb，那么，这里就不再设置,直接返回错误
        if unlikely(!p.is_null()) {
            return Err(SystemError::EPERM);
        }
        // 将pcb的地址放到内核栈的最低地址处
        unsafe {
            *(self.stack.as_ptr() as *mut *const ProcessControlBlock) = p;
        }

        return Ok(());
    }

    /// 返回指向当前内核栈pcb的Arc指针
    pub unsafe fn pcb(&self) -> Option<Arc<ProcessControlBlock>> {
        // 从内核栈的最低地址处取出pcb的地址
        let p = self.stack.as_ptr() as *const ProcessControlBlock;
        if unlikely(p.is_null()) {
            return None;
        }

        // 为了防止内核栈的pcb指针被释放，这里需要将其包装一下，使得Arc的drop不会被调用
        let arc_wrapper: ManuallyDrop<Arc<ProcessControlBlock>> =
            ManuallyDrop::new(Arc::from_raw(p));

        let new_arc: Arc<ProcessControlBlock> = Arc::clone(&arc_wrapper);
        return Some(new_arc);
    }
}

impl Drop for KernelStack {
    fn drop(&mut self) {
        let pcb_ptr: Arc<ProcessControlBlock> =
            unsafe { Arc::from_raw(self.stack.as_ptr() as *const ProcessControlBlock) };
        drop(pcb_ptr);
    }
}

pub fn process_init() {
    unsafe {
        compiler_fence(Ordering::SeqCst);
        current_pcb().address_space = null_mut();
        kdebug!("To create address space for INIT process.");
        // test_buddy();
        set_INITIAL_PROCESS_ADDRESS_SPACE(
            AddressSpace::new(true).expect("Failed to create address space for INIT process."),
        );
        kdebug!("INIT process address space created.");
        compiler_fence(Ordering::SeqCst);
        current_pcb().set_address_space(INITIAL_PROCESS_ADDRESS_SPACE());
        compiler_fence(Ordering::SeqCst);
    };
}
