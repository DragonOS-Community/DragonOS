//! 进程/调度类 tracepoint 声明。
//!
//! 字段与 raw ABI 对齐 Linux 6.6 `include/trace/events/sched.h`。

use crate::unsafe_define_event_trace;

// SAFETY: sched_process_exec is emitted from the process exec path and is not
// reachable from x86 NMI/MCE handlers.
unsafe_define_event_trace!(
    sched_process_exec,
    TP_system(sched),
    TP_PROTO(filename: &[u8], pid: i32, old_pid: i32),
    TP_STRUCT__entry {
        __string(filename, filename),
        pid: i32,
        old_pid: i32,
    },
    TP_fast_assign {
        pid: pid,
        old_pid: old_pid,
    },
    TP_ident(__entry),
    TP_printk({
        let filename = alloc::string::String::from_utf8_lossy(__entry.filename);
        format!(
            "filename={} pid={} old_pid={}",
            filename, __entry.pid, __entry.old_pid
        )
    }),
    TP_print_fmt("\"filename=%s pid=%d old_pid=%d\", __get_str(filename), REC->pid, REC->old_pid")
);

/// Copy a comm string into a fixed 16-byte field, NUL-terminated like
/// Linux `TASK_COMM_LEN` handling (truncation at 15 chars).
fn comm_to_array(comm: &[u8]) -> [u8; 16] {
    let mut buf = [0u8; 16];
    let len = comm.len().min(15);
    buf[..len].copy_from_slice(&comm[..len]);
    buf
}

// SAFETY: sched_process_exit is emitted from the process exit path
// (ProcessManager::exit) in process context and is not reachable from x86
// NMI/MCE handlers.
unsafe_define_event_trace!(
    sched_process_exit,
    TP_system(sched),
    TP_PROTO(comm: &[u8], pid: i32, prio: i32),
    TP_STRUCT__entry {
        comm: [u8; 16],
        pid: i32,
        prio: i32,
    },
    TP_fast_assign {
        comm: comm_to_array(comm),
        pid: pid,
        prio: prio,
    },
    TP_ident(__entry),
    TP_printk({
        let comm = core::str::from_utf8(&__entry.comm)
            .unwrap_or("")
            .trim_end_matches('\0');
        format!("comm={} pid={} prio={}", comm, __entry.pid, __entry.prio)
    })
);
