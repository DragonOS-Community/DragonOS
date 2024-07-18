use crate::arch::interrupt::TrapFrame;
use crate::arch::kprobe::clear_single_step;
use crate::debug::kprobe::DEBUG_KPROBE_LIST;
use kprobe::{KprobeOps, ProbeArgs};
use system_error::SystemError;

#[derive(Debug)]
pub struct DebugException;

impl DebugException {
    pub fn handle(frame: &mut TrapFrame) -> Result<(), SystemError> {
        Self::post_kprobe_handler(frame)
    }

    fn post_kprobe_handler(frame: &mut TrapFrame) -> Result<(), SystemError> {
        let pc = frame.debug_address();
        let kprobe = DEBUG_KPROBE_LIST.lock().get(&pc).map(Clone::clone);
        if let Some(kprobe) = kprobe {
            kprobe.call_post_handler(frame);
            clear_single_step(frame, kprobe.return_address());
        } else {
            println!("There is no kprobe on pc {:#x}", pc);
        }
        Ok(())
    }
}
