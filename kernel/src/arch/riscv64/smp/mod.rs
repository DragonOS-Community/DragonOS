use system_error::SystemError;

use crate::{
    kwarn,
    smp::{
        cpu::{CpuHpCpuState, ProcessorId},
        SMPArch,
    },
};

pub struct RiscV64SMPArch;

impl SMPArch for RiscV64SMPArch {
    #[inline(never)]
    fn prepare_cpus() -> Result<(), SystemError> {
        kwarn!("RiscV64SMPArch::prepare_cpus() is not implemented");
        Ok(())
    }

    fn start_cpu(_cpu_id: ProcessorId, _hp_state: &CpuHpCpuState) -> Result<(), SystemError> {
        kwarn!("RiscV64SMPArch::start_cpu() is not implemented");
        Ok(())
    }
}
