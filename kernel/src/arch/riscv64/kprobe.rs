use crate::arch::interrupt::TrapFrame;

pub fn setup_single_step(frame: &mut TrapFrame, step_addr: usize) {
    frame.set_pc(step_addr);
}

pub fn clear_single_step(frame: &mut TrapFrame, return_addr: usize) {
    frame.set_pc(return_addr);
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct KProbeContext {
    pub pc: usize,
    pub ra: usize,
    pub sp: usize,
    pub gp: usize,
    pub tp: usize,
    pub t0: usize,
    pub t1: usize,
    pub t2: usize,
    pub s0: usize,
    pub s1: usize,
    pub a0: usize,
    pub a1: usize,
    pub a2: usize,
    pub a3: usize,
    pub a4: usize,
    pub a5: usize,
    pub a6: usize,
    pub a7: usize,
    pub s2: usize,
    pub s3: usize,
    pub s4: usize,
    pub s5: usize,
    pub s6: usize,
    pub s7: usize,
    pub s8: usize,
    pub s9: usize,
    pub s10: usize,
    pub s11: usize,
    pub t3: usize,
    pub t4: usize,
    pub t5: usize,
    pub t6: usize,
}

impl From<&TrapFrame> for KProbeContext {
    fn from(trap_frame: &TrapFrame) -> Self {
        Self {
            pc: trap_frame.epc,
            ra: trap_frame.ra,
            sp: trap_frame.sp,
            gp: trap_frame.gp,
            tp: trap_frame.tp,
            t0: trap_frame.t0,
            t1: trap_frame.t1,
            t2: trap_frame.t2,
            s0: trap_frame.s0,
            s1: trap_frame.s1,
            a0: trap_frame.a0,
            a1: trap_frame.a1,
            a2: trap_frame.a2,
            a3: trap_frame.a3,
            a4: trap_frame.a4,
            a5: trap_frame.a5,
            a6: trap_frame.a6,
            a7: trap_frame.a7,
            s2: trap_frame.s2,
            s3: trap_frame.s3,
            s4: trap_frame.s4,
            s5: trap_frame.s5,
            s6: trap_frame.s6,
            s7: trap_frame.s7,
            s8: trap_frame.s8,
            s9: trap_frame.s9,
            s10: trap_frame.s10,
            s11: trap_frame.s11,
            t3: trap_frame.t3,
            t4: trap_frame.t4,
            t5: trap_frame.t5,
            t6: trap_frame.t6,
        }
    }
}
