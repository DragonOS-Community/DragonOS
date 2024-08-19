use crate::arch::interrupt::TrapFrame;
use crate::arch::kprobe::clear_single_step;
use crate::debug::kprobe::KPROBE_MANAGER;
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
        if let Some(kprobe_list) = KPROBE_MANAGER.lock().get_debug_list(pc) {
            for kprobe in kprobe_list {
                kprobe.call_post_handler(frame);
            }
            let probe_point = kprobe_list[0].probe_point();
            clear_single_step(frame, probe_point.return_address());
        } else {
            println!("There is no kprobe on pc {:#x}", pc);
        }
        Ok(())
    }
}
