use alloc::vec::Vec;

use crate::{
    init::boot_params,
    kdebug,
    mm::percpu::{PerCpu, PerCpuVar},
    smp::cpu::{ProcessorId, SmpCpuManager},
};

/// 获取当前cpu的id
#[inline]
pub fn current_cpu_id() -> ProcessorId {
    let ptr: *const LocalContext = riscv::register::sscratch::read() as *const LocalContext;

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
/// 每个CPU的sscratch寄存器指向这个结构体
#[derive(Debug)]
pub(super) struct LocalContext {
    /// 当前cpu的id
    current_cpu: ProcessorId,
}

impl LocalContext {
    fn new(cpu: ProcessorId) -> Self {
        Self { current_cpu: cpu }
    }
    pub fn current_cpu(&self) -> ProcessorId {
        self.current_cpu
    }

    pub fn set_current_cpu(&mut self, cpu: ProcessorId) {
        self.current_cpu = cpu;
    }

    fn sync_to_cpu(&self) {
        let ptr = self as *const Self as usize;
        riscv::register::sscratch::write(ptr);
    }
}

/// 初始化本地上下文
#[inline(never)]
pub(super) fn init_local_context() {
    kdebug!("init_local_context");
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
