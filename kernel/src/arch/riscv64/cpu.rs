use alloc::vec::Vec;

use crate::{
    init::boot_params,
    kdebug,
    mm::percpu::{PerCpu, PerCpuVar},
    smp::cpu::{ProcessorId, SmpCpuManager},
};

/// 栈对齐
pub(super) const STACK_ALIGN: usize = 16;

/// 获取当前cpu的id
#[inline]
pub fn current_cpu_id() -> ProcessorId {
    let ptr: *const LocalContext = riscv::register::tp::read() as *const LocalContext;

    if core::intrinsics::unlikely(ptr.is_null()) {
        return boot_params().read_irqsave().arch.boot_hartid;
    }

    unsafe { (*ptr).current_cpu() }
}

/// 重置cpu
pub unsafe fn cpu_reset() -> ! {
    sbi_rt::system_reset(sbi_rt::WarmReboot, sbi_rt::NoReason);
    unimplemented!("RiscV64 reset failed, manual override expected ...")
}

static mut LOCAL_CONTEXT: Option<PerCpuVar<LocalContext>> = None;

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
#[derive(Debug)]
pub(super) struct LocalContext {
    /// 当前cpu的id
    pub current_cpu: ProcessorId,
    // 当前进程的内核栈指针（暂存，当进入中断处理程序的时候需要保存到pcb，进程切换的时候需要重新设置这个值）
    pub kernel_sp: usize,
    // 当前进程的用户栈指针（暂存，当进入中断处理程序的时候需要保存到pcb，进程切换的时候需要重新设置这个值）
    pub user_sp: usize,
}

impl LocalContext {
    fn new(cpu: ProcessorId) -> Self {
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
    pub fn arch_init(boot_cpu: ProcessorId) {
        // todo: 读取所有可用的CPU
    }
}
