use crate::arch::interrupt::TrapFrame;

pub fn setup_single_step(frame: &mut TrapFrame, step_addr: usize) {
    todo!("la64: setup_single_step")
}

pub fn clear_single_step(frame: &mut TrapFrame, return_addr: usize) {
    todo!("la64: clear_single_step")
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct KProbeContext {}

impl From<&TrapFrame> for KProbeContext {
    fn from(trap_frame: &TrapFrame) -> Self {
        todo!("from trap frame to kprobe context");
    }
}
