use system_error::SystemError;

use crate::smp::{
    cpu::{CpuHpCpuState, ProcessorId},
    SMPArch,
};

pub struct RiscV64SMPArch;

impl SMPArch for RiscV64SMPArch {
    #[inline(never)]
    fn prepare_cpus() -> Result<(), SystemError> {
        todo!()
    }

    fn start_cpu(cpu_id: ProcessorId, hp_state: &CpuHpCpuState) -> Result<(), SystemError> {
        todo!()
    }
}
