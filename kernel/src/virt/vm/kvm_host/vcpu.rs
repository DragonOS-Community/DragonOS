use alloc::{string::String, sync::Arc};

use crate::{
    process::{Pid, ProcessManager},
    smp::cpu::ProcessorId,
};

use super::{KvmMemSlots, Vm};

pub struct VirtCpu {
    cpu: ProcessorId,
    kvm: Arc<Vm>,
    vcpu_id: usize,
    pid: Option<Pid>,
    preempted: bool,
    ready: bool,
    last_used_slot: Option<Arc<KvmMemSlots>>,
    stats_id: String,
}

impl VirtCpu {
    /// ### 创建一个vcpu，并且初始化部分数据
    pub fn create(vm: Arc<Vm>, id: usize) -> Self {
        Self {
            cpu: ProcessorId::INVALID,
            kvm: vm,
            vcpu_id: id,
            pid: None,
            preempted: false,
            ready: false,
            last_used_slot: None,
            stats_id: format!("kvm-{}/vcpu-{}", ProcessManager::current_pid().data(), id),
        }
    }
}

/// ## 多处理器状态（有些状态在某些架构并不合法）
#[derive(Debug, Clone, Copy)]
pub enum MutilProcessorState {
    Runnable,
    Uninitialized,
    InitReceived,
    Halted,
    SipiReceived,
    Stopped,
    CheckStop,
    Operating,
    Load,
    ApResetHold,
    Suspended,
}
