//! Syscall tracepoints aligned with Linux 6.6 `include/trace/events/syscalls.h`.
//!
//! Raw ABI (BPF ctx) layout, byte-identical to Linux
//! `struct trace_event_raw_sys_enter/exit`:
//!
//! sys_enter:
//! ```text
//! offset 0..8   : struct trace_entry (common_type/flags/preempt_count/pid)
//! offset 8      : int id (syscall nr, 4 bytes)
//! offset 12..16 : padding
//! offset 16..64 : unsigned long args[6]
//! ```
//!
//! sys_exit:
//! ```text
//! offset 0..8 : struct trace_entry
//! offset 8    : long ret
//! ```
//!
//! DragonOS packs the payload after the 8-byte `TraceEntry`, so an explicit
//! padding field keeps `args` at offset 16 exactly like Linux.

use crate::unsafe_define_event_trace;

/// Arguments shared by the syscall-enter tracepoints (6 raw register values).
pub type SyscallTraceArgs = [u64; 6];

// SAFETY: sys_enter_execve is emitted from syscall process context and is not
// reachable from x86 NMI/MCE handlers.
unsafe_define_event_trace!(
    sys_enter_execve,
    TP_system(syscalls),
    TP_PROTO(id: i32, args: SyscallTraceArgs),
    TP_STRUCT__entry {
        id: i32,
        __pad: u32,
        args: [u64; 6],
    },
    TP_fast_assign {
        id: id,
        __pad: 0,
        args: args,
    },
    TP_ident(__entry),
    TP_printk({
        format!(
            "id: {}, args: {:?}",
            __entry.id, __entry.args
        )
    })
);

// SAFETY: sys_exit_execve is emitted from syscall process context and is not
// reachable from x86 NMI/MCE handlers.
unsafe_define_event_trace!(
    sys_exit_execve,
    TP_system(syscalls),
    TP_PROTO(ret: i64),
    TP_STRUCT__entry {
        ret: i64,
    },
    TP_fast_assign {
        ret: ret,
    },
    TP_ident(__entry),
    TP_printk({
        format!("ret: {}", __entry.ret)
    })
);

// SAFETY: sys_enter_write is emitted from syscall process context and is not
// reachable from x86 NMI/MCE handlers.
unsafe_define_event_trace!(
    sys_enter_write,
    TP_system(syscalls),
    TP_PROTO(id: i32, args: SyscallTraceArgs),
    TP_STRUCT__entry {
        id: i32,
        __pad: u32,
        args: [u64; 6],
    },
    TP_fast_assign {
        id: id,
        __pad: 0,
        args: args,
    },
    TP_ident(__entry),
    TP_printk({
        format!(
            "id: {}, args: {:?}",
            __entry.id, __entry.args
        )
    })
);
