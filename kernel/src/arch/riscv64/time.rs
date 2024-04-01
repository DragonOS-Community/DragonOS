use crate::time::TimeArch;
pub struct RiscV64TimeArch;

impl TimeArch for RiscV64TimeArch {
    fn get_cycles() -> usize {
        riscv::register::cycle::read()
    }

    fn cal_expire_cycles(ns: usize) -> usize {
        todo!()
    }

    fn get_cycles_ns() -> usize {
        todo!()
    }
}
