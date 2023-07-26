use core::{
    ptr::null_mut,
    sync::atomic::{compiler_fence, Ordering, AtomicUsize}, intrinsics::unlikely, mem::ManuallyDrop,
};

use alloc::{sync::Arc, boxed::Box};

use crate::{
    arch::{asm::current::current_pcb, fpu::FpState, process::ArchPCBInfo},
    kdebug,
    mm::{
        set_INITIAL_PROCESS_ADDRESS_SPACE, ucontext::AddressSpace, INITIAL_PROCESS_ADDRESS_SPACE,
    }, libs::{align::AlignedBox, rwlock::RwLock}, syscall::SystemError, filesystem::vfs::file::FileDescriptorVec, ipc::signal_types::{signal_struct, sighand_struct, sigset_t, sigpending, SignalNumber}, sched::SchedPolicy,
};

pub mod abi;
pub mod c_adapter;
pub mod exec;
pub mod fork;
pub mod initial_proc;
pub mod pid;
pub mod preempt;
pub mod process;
pub mod syscall;
pub mod kthread;
pub mod wait_queue;

const PCB_NAME_LEN: usize = 16;

int_like!(Pid, AtomicPid, usize, AtomicUsize);

#[derive(Debug,PartialEq)]
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
    inner: RwLock<InnerProcessControlBlock>,
}

// https://opengrok.ringotek.cn/xref/DragonOS/kernel/src/process/proc-types.h?r=64aea4b3
#[derive(Debug)]
pub struct InnerProcessControlBlock {
    /// 当前进程的pid
    pub pid: Pid,
    /// 当前进程的进程组id
    pub pgid: Pid,
    /// 当前进程的父进程的pid
    pub ppid: Pid,
    /// 当前进程所在的cpu
    pub on_cpu: usize,
    /// 标志位
    pub flags:ProcessFlags,
    /* PF_kTHREAD  | PF_IO_WORKER 的进程，worker_private不为NULL*/
    pub worker_private: Option<*mut ()>,
    /// 当前进程的自旋锁持有计数
    pub preempt_count: AtomicUsize,
    /// 当前进程的状态
    pub state: ProcessState,
    /// 进程的调度策略
    pub sched_policy: SchedPolicy,
    /// 当前进程的虚拟运行时间
    pub virtual_runtime: isize,
    /// 由实时调度器管理的时间片
    pub rt_time_slice: isize,
    /// 进程的名字
    pub name: Arc<RwLock<Box<str>>>,
    /// 与处理器架构相关的信息
    pub arch: ArchPCBInfo,
    /// 进程的内核栈
    pub kernel_stack: Option<KernelStack>,
    /// 文件描述符表
    pub fd_table: Arc<RwLock<FileDescriptorVec>>,
    /// 如果当前进程等待被迁移到另一个cpu核心上（也就是flags中的PF_NEED_MIGRATE被置位），
    /// 该字段存储要被迁移到的目标处理器核心号
    pub migrate_to: u32,


    // 信号相关的字段。由于信号机制实现的不是很好，因此这里使用了裸指针来避免所有权问题。
    // TODO：重构信号机制。
    pub signal: *mut signal_struct,
    pub sighand: *mut sighand_struct,
    pub sig_blocked: sigset_t,
    pub sig_pending: sigpending,

    /// 浮点寄存器的状态
    pub fp_state: Option<FpState>,
    // todo: 待内存管理完成后，增加地址空间相关的字段
}

impl InnerProcessControlBlock {}

/// 生成一个新的pid
pub fn generate_pid() -> Pid {
    static NEXT_PID: AtomicPid = AtomicPid::new(Pid(1));
    return NEXT_PID.fetch_add(Pid(1), Ordering::SeqCst);
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
