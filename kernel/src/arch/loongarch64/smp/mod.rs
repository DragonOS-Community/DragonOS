use log::warn;
use system_error::SystemError;

use crate::smp::{
    cpu::{CpuHpCpuState, ProcessorId, SmpCpuManager},
    SMPArch,
};

pub struct LoongArch64SMPArch;

impl SMPArch for LoongArch64SMPArch {
    #[inline(never)]
    fn prepare_cpus() -> Result<(), SystemError> {
        warn!("LoongArch64SMPArch::prepare_cpus() is not implemented");
        Ok(())
    }

    fn start_cpu(_cpu_id: ProcessorId, _hp_state: &CpuHpCpuState) -> Result<(), SystemError> {
        warn!("LoongArch64SMPArch::start_cpu() is not implemented");
        Ok(())
    }
}

impl SmpCpuManager {
    pub fn arch_init(_boot_cpu: ProcessorId) {
        // todo: 读取所有可用的CPU
        todo!("la64:SmpCpuManager::arch_init()")
    }
}
