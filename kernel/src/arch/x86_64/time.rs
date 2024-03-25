use crate::time::TimeArch;

use super::driver::tsc::TSCManager;

pub struct X86_64TimeArch;

impl TimeArch for X86_64TimeArch {
    fn get_cycles() -> usize {
        unsafe { x86::time::rdtsc() as usize }
    }

    fn cal_expire_cycles(ns: usize) -> usize {
        Self::get_cycles() + ns * TSCManager::cpu_khz() as usize / 1000000
    }
}
