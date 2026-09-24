use alloc::vec::Vec;
use sbi_rt::HartMask;

use crate::{
    init::boot_params,
    mm::percpu::{PerCpu, PerCpuVar},
    smp::cpu::{ProcessorId, SmpCpuManager},
};

/// 栈对齐
pub(super) const STACK_ALIGN: usize = 16;

/// RISC-V的XLEN，也就是寄存器的位宽
pub const RISCV_XLEN: usize = core::mem::size_of::<usize>() * 8;

/// 获取当前cpu的id
#[inline]
pub fn current_cpu_id() -> ProcessorId {
    let ptr: *const LocalContext = riscv::register::tp::read() as *const LocalContext;

    if core::intrinsics::unlikely(ptr.is_null()) {
        return ProcessorId::new(unsafe { super::init::BOOT_HARTID });
    }

    unsafe { (*ptr).current_cpu() }
}
impl Into<HartMask> for ProcessorId {
    fn into(self) -> HartMask {
        let hart = self.data() as usize;
        let base = (hart / RISCV_XLEN) * RISCV_XLEN;
        let offset = hart - base;
        HartMask::from_mask_base(1usize << offset, base)
    }
}
/// 重置cpu
pub unsafe fn cpu_reset() -> ! {
    sbi_rt::system_reset(sbi_rt::WarmReboot, sbi_rt::NoReason);
    unimplemented!("RiscV64 reset failed, manual override expected ...")
}

static mut LOCAL_CONTEXT: Option<PerCpuVar<LocalContext>> = None;

/// 早期启动（堆尚未就绪、`tp` 还未指向堆上的 `LocalContext`）时使用的静态上下文。
///
/// `setup_trap_vector()` 一旦安装，任何同步异常都会进入 `handle_exception`，
/// 而 trap 入口会用 `tp` 去访问 `LocalContext`。堆上的 `LOCAL_CONTEXT` 要等到
/// `mm_init` 之后才建立，所以这里在安装 trap 向量之前先把 `tp` 指向一个静态
/// 上下文，避免早期异常在 trap 入口解引用空 `tp` 而无限递归。
static mut BOOT_LOCAL_CONTEXT: core::mem::MaybeUninit<LocalContext> =
    core::mem::MaybeUninit::uninit();

/// 在安装 trap 向量之前，把 `tp` 指向静态的启动上下文。
///
/// # Safety
/// 只能在启动 hart 上、且在 `init_local_context()` 建立堆上上下文之前调用一次。
pub(super) unsafe fn init_boot_local_context(cpu: ProcessorId) {
    let ctx = &raw mut BOOT_LOCAL_CONTEXT;
    (*ctx).write(LocalContext::new(cpu));
    riscv::register::sscratch::write(0);
    riscv::register::tp::write(ctx as usize);
}

#[inline(always)]
pub(super) fn local_context() -> &'static PerCpuVar<LocalContext> {
    unsafe { LOCAL_CONTEXT.as_ref().unwrap() }
}

/// Per cpu的上下文数据
///
/// 每个CPU的tp寄存器指向这个结构体
///
/// 注意：
///
/// - 从用户态进入内核态时，会从sscratch寄存器加载这个结构体的地址到tp寄存器，并把sscratch寄存器清零
/// - 从内核态进入用户态时，会将tp寄存器的值保存到sscratch寄存器
#[derive(Debug, Clone, Copy)]
pub(super) struct LocalContext {
    /// 当前cpu的id
    pub current_cpu: ProcessorId,
    // 当前进程的内核栈指针（暂存，当进入中断处理程序的时候需要保存到pcb，进程切换的时候需要重新设置这个值）
    pub kernel_sp: usize,
    // 当前进程的用户栈指针（暂存，当进入中断处理程序的时候需要保存到pcb，进程切换的时候需要重新设置这个值）
    pub user_sp: usize,
}

#[allow(dead_code)]
impl LocalContext {
    pub fn new(cpu: ProcessorId) -> Self {
        Self {
            current_cpu: cpu,
            kernel_sp: 0,
            user_sp: 0,
        }
    }
    pub fn current_cpu(&self) -> ProcessorId {
        self.current_cpu
    }

    pub fn set_current_cpu(&mut self, cpu: ProcessorId) {
        self.current_cpu = cpu;
    }

    pub fn kernel_sp(&self) -> usize {
        self.kernel_sp
    }

    pub fn set_kernel_sp(&mut self, sp: usize) {
        self.kernel_sp = sp;
    }

    pub fn user_sp(&self) -> usize {
        self.user_sp
    }

    pub fn set_user_sp(&mut self, sp: usize) {
        self.user_sp = sp;
    }

    fn sync_to_cpu(&self) {
        let ptr = self as *const Self as usize;
        riscv::register::sscratch::write(0);

        // 写入tp寄存器
        riscv::register::tp::write(ptr);
    }

    pub fn restore(&mut self, from: &LocalContext) {
        // 不恢复cpu id

        self.kernel_sp = from.kernel_sp;
        self.user_sp = from.user_sp;
    }
}

/// 初始化本地上下文
#[inline(never)]
pub(super) fn init_local_context() {
    let mut data = Vec::new();

    for i in 0..PerCpu::MAX_CPU_NUM {
        data.push(LocalContext::new(ProcessorId::new(i)));
    }
    let ctx = PerCpuVar::new(data).unwrap();

    unsafe {
        LOCAL_CONTEXT = Some(ctx);
    }

    let hartid = boot_params().read().arch.boot_hartid;

    let ctx = unsafe { local_context().force_get(hartid) };
    ctx.sync_to_cpu();
}

impl SmpCpuManager {
    pub fn arch_init(_boot_cpu: ProcessorId) {
        // todo: 读取所有可用的CPU
    }
}
