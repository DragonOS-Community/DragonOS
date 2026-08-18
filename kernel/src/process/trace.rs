//! 进程/调度类 tracepoint 声明。
//!
//! 字段与 raw ABI 对齐 Linux 6.6 `include/trace/events/sched.h`。

use crate::define_event_trace;

define_event_trace!(
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
