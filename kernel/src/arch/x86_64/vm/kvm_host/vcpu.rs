use crate::{
    smp::cpu::ProcessorId,
    virt::vm::kvm_host::vcpu::{MutilProcessorState, VirtCpu},
};

#[derive(Debug)]
pub struct X86VcpuArch {
    /// 最近一次尝试进入虚拟机的主机cpu
    last_vmentry_cpu: ProcessorId,
    /// 可用寄存器数量
    regs_avail: u32,
    /// 脏寄存器数量
    regs_dirty: u32,
    /// 多处理器状态
    mp_state: MutilProcessorState,
}

impl VirtCpu {
    pub fn init_arch(&mut self) {}
}

impl Default for X86VcpuArch {
    fn default() -> Self {
        Self {
            last_vmentry_cpu: ProcessorId::INVALID,
            regs_avail: !0,
            regs_dirty: !0,
            mp_state: MutilProcessorState::Runnable,
        }
    }
}
