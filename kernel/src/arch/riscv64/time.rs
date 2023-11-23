use crate::time::TimeArch;
pub struct RiscV64TimeArch;

impl TimeArch for RiscV64TimeArch {
    fn get_cycles() -> usize {
        unimplemented!("Riscv64TimeArch::get_cycles")
    }
}
