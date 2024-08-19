use crate::arch::interrupt::TrapFrame;

pub fn setup_single_step(frame: &mut TrapFrame, step_addr: usize) {
    frame.rflags |= 0x100;
    frame.set_pc(step_addr);
}

pub fn clear_single_step(frame: &mut TrapFrame, return_addr: usize) {
    frame.rflags &= !0x100;
    frame.set_pc(return_addr);
}
