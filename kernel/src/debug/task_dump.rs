//! Administrator-requested task metadata, without inspecting another task's stack.

use crate::{
    debug::klog::loglevel::LogLevel, libs::printk::Logger, process::snapshot_all_processes,
};
use system_error::SystemError;

/// Called from process context by the privileged SysRq proc handler. The Arc
/// snapshot pins task metadata, but does not stop tasks or stabilize their stacks.
/// Records use the existing bounded, fallible kmsg sink even when the console
/// loglevel suppresses informational messages. Output remains best effort.
pub(crate) fn dump_all_tasks() -> Result<(), SystemError> {
    let tasks = snapshot_all_processes()?;
    Logger.try_log_fmt(
        LogLevel::INFO,
        format_args!("SysRq : Show State ({} tasks)", tasks.len()),
    );
    for pcb in tasks {
        let mut comm = [0u8; 16];
        let len = {
            let basic = pcb.basic();
            let name = basic.name();
            let mut len = name.len().min(comm.len());
            while !name.is_char_boundary(len) {
                len -= 1;
            }
            comm[..len].copy_from_slice(&name.as_bytes()[..len]);
            len
        };
        let state = pcb.sched_info().state();
        let name = core::str::from_utf8(&comm[..len]).expect("task name UTF-8 boundary");
        // No task lock is held during formatting, logging, or allocation.
        Logger.try_log_fmt(
            LogLevel::INFO,
            format_args!(
                "[pid {:>6}] {:<16} state={:?}",
                pcb.raw_pid().data(),
                name,
                state
            ),
        );
    }
    Logger.try_log_text(LogLevel::INFO, "SysRq : End Show State");
    Ok(())
}
