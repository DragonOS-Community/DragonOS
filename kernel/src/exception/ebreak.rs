use crate::arch::interrupt::TrapFrame;
use crate::arch::kprobe::setup_single_step;
use crate::debug::kprobe::BREAK_KPROBE_LIST;
use crate::exception::debug::DebugException;
use kprobe::{KprobeOps, ProbeArgs};
use system_error::SystemError;

#[derive(Debug)]
pub struct EBreak;

impl EBreak {
    pub fn handle(frame: &mut TrapFrame) -> Result<(), SystemError> {
        Self::kprobe_handler(frame)
    }
    fn kprobe_handler(frame: &mut TrapFrame) -> Result<(), SystemError> {
        let break_addr = frame.break_address();
        let kprobe = BREAK_KPROBE_LIST.lock().get(&break_addr).map(Clone::clone);
        if let Some(kprobe) = kprobe {
            kprobe.call_pre_handler(frame);
            // setup_single_step
            setup_single_step(frame, kprobe.single_step_address());
        } else {
            // For some architectures, they do not support single step execution,
            // and we need to use breakpoint exceptions to simulate
            DebugException::handle(frame)?;
        }
        Ok(())
    }
}
