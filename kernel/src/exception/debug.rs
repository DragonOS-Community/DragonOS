use crate::arch::interrupt::TrapFrame;
use crate::arch::kprobe::clear_single_step;
use crate::debug::kprobe::KPROBE_MANAGER;
use kprobe::{KprobeOps, ProbeArgs};
use log::debug;
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
                let guard = kprobe.read();
                if guard.is_enabled() {
                    guard.call_post_handler(frame);
                    guard.call_event_callback(frame);
                }
            }
            let return_address = kprobe_list[0].read().probe_point().return_address();
            clear_single_step(frame, return_address);
        } else {
            debug!("There is no kprobe on pc {:#x}", pc);
        }
        Ok(())
    }
}
