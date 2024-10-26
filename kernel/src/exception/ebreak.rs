use crate::arch::interrupt::TrapFrame;
use crate::arch::kprobe::setup_single_step;
use crate::debug::kprobe::KPROBE_MANAGER;
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
        let guard = KPROBE_MANAGER.lock();
        let kprobe_list = guard.get_break_list(break_addr);
        if let Some(kprobe_list) = kprobe_list {
            for kprobe in kprobe_list {
                let guard = kprobe.read();
                if guard.is_enabled() {
                    guard.call_pre_handler(frame);
                }
            }
            let single_step_address = kprobe_list[0].read().probe_point().single_step_address();
            // setup_single_step
            setup_single_step(frame, single_step_address);
        } else {
            // For some architectures, they do not support single step execution,
            // and we need to use breakpoint exceptions to simulate
            drop(guard);
            DebugException::handle(frame)?;
        }
        Ok(())
    }
}
