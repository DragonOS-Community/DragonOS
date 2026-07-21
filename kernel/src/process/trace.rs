//! 进程/调度类 tracepoint 声明。
//!
//! 字段参考 Linux `include/trace/events/sched.h`。
//! 注意：DragonOS 的 `define_event_trace!` 宏不支持 Linux 的 `__string` 动态字符串，
//! 且当前未实现 `bpf_get_current_comm()` helper，故 `comm` 直接放入 payload。

use crate::define_event_trace;

define_event_trace!(
    sched_process_exec,
    TP_system(sched),
    TP_PROTO(comm: &str, pid: i32, old_pid: i32),
    TP_STRUCT__entry {
        comm: [u8; 16],
        pid: i32,
        old_pid: i32,
    },
    TP_fast_assign {
        comm: {
            // 对齐 Linux TASK_COMM_LEN=16（15 字符 + NUL）。
            let mut buf = [0u8; 16];
            let bytes = comm.as_bytes();
            let len = bytes.len().min(15);
            buf[..len].copy_from_slice(&bytes[..len]);
            buf
        },
        pid: pid,
        old_pid: old_pid,
    },
    TP_ident(__entry),
    TP_printk({
        let nul = __entry.comm.iter().position(|&b| b == 0).unwrap_or(16);
        let comm = core::str::from_utf8(&__entry.comm[..nul]).unwrap_or("invalid utf8");
        format!("comm={} pid={} old_pid={}", comm, __entry.pid, __entry.old_pid)
    })
);
