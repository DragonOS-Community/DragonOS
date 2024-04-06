use bitmap::traits::BitMapOps;
use system_error::SystemError;

use crate::syscall::Syscall;

use super::cpu::smp_cpu_manager;

impl Syscall {
    pub fn getaffinity(_pid: i32, set: &mut [u8]) -> Result<usize, SystemError> {
        let cpu_manager = smp_cpu_manager();
        let src = unsafe { cpu_manager.possible_cpus().inner().as_bytes() };
        set[0..src.len()].copy_from_slice(src);
        Ok(0)
    }
}
