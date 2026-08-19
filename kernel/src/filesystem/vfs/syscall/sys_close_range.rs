//! Linux-compatible `close_range(2)` implementation.

use alloc::{string::ToString, sync::Arc, vec::Vec};

use system_error::SystemError;

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_CLOSE_RANGE},
    filesystem::vfs::file::{FileDescriptorTable, FileDescriptorVec},
    process::ProcessManager,
    sched::sched_yield,
    syscall::table::{FormattedSyscallParam, Syscall},
};

bitflags! {
    struct CloseRangeFlags: u32 {
        const UNSHARE = 1 << 1;
        const CLOEXEC = 1 << 2;
    }
}

/// Keep the fd-table write lock and the current task on-CPU for at most this
/// many scanned slots before a voluntary scheduling point.
const CLOSE_RANGE_WORK_BUDGET: usize = 256;

pub struct SysCloseRangeHandle;

impl Syscall for SysCloseRangeHandle {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        do_close_range(args[0] as u32, args[1] as u32, args[2] as u32)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("first", (args[0] as u32).to_string()),
            FormattedSyscallParam::new("last", (args[1] as u32).to_string()),
            FormattedSyscallParam::new("flags", alloc::format!("{:#x}", args[2] as u32)),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_CLOSE_RANGE, SysCloseRangeHandle);

fn close_range_in_table(table: &Arc<FileDescriptorTable>, first: u32, last: u32) {
    let Some(end) = table.read().close_range_end(last) else {
        return;
    };
    let mut cursor = first as usize;
    if cursor > end {
        return;
    }

    let mut work = 0usize;
    loop {
        let remaining_budget = CLOSE_RANGE_WORK_BUDGET - work;
        let scan = table
            .write()
            .take_next_open_in_range(cursor, end, remaining_budget.max(1));
        cursor = scan.next;
        work += scan.scanned;

        if let Some(dropped) = scan.dropped {
            // Linux ignores individual filp_close() errors in __range_close().
            let _ = dropped.finish_close();
        }

        if scan.done {
            break;
        }
        if work >= CLOSE_RANGE_WORK_BUDGET {
            sched_yield();
            work = 0;
        }
    }
}

fn set_cloexec_in_table(table: &Arc<FileDescriptorTable>, first: u32, last: u32) {
    table.write().set_cloexec_range(first, last);
}

fn do_close_range(first: u32, last: u32, flags: u32) -> Result<usize, SystemError> {
    let flags = CloseRangeFlags::from_bits(flags).ok_or(SystemError::EINVAL)?;
    if first > last {
        return Err(SystemError::EINVAL);
    }

    let current = ProcessManager::current_pcb();
    let (old_table, shared_by_tasks) = current
        .basic()
        .fd_table_snapshot()
        .expect("close_range task has no fd table");
    let must_unshare = flags.contains(CloseRangeFlags::UNSHARE) && shared_by_tasks;

    if must_unshare {
        let punch_hole = if flags.contains(CloseRangeFlags::CLOEXEC) {
            None
        } else {
            Some((first, last))
        };
        let new_table = FileDescriptorVec::try_clone_for_close_range(&old_table, punch_hole)?;

        if flags.contains(CloseRangeFlags::CLOEXEC) {
            set_cloexec_in_table(&new_table, first, last);
        } else {
            close_range_in_table(&new_table, first, last);
        }

        let replaced = {
            let mut basic = current.basic_mut();
            basic.set_fd_table(Some(new_table))
        };
        // A final fd-table drop can flush files. Never do it under basic/fdtable locks.
        drop(replaced);
        drop(old_table);
    } else if flags.contains(CloseRangeFlags::CLOEXEC) {
        set_cloexec_in_table(&old_table, first, last);
    } else {
        close_range_in_table(&old_table, first, last);
    }

    Ok(0)
}
