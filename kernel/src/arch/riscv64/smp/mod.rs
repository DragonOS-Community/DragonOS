use system_error::SystemError;

use crate::smp::SMPArch;

pub struct RiscV64SMPArch;

impl SMPArch for RiscV64SMPArch {
    fn prepare_cpus() -> Result<(), SystemError> {
        todo!()
    }

    fn init() -> Result<(), SystemError> {
        todo!()
    }
}
