use crate::time::TimeArch;

pub struct X86_64TimeArch;

impl TimeArch for X86_64TimeArch {
    fn get_cycles() -> usize {
        unsafe { x86::time::rdtsc() as usize }
    }
}
