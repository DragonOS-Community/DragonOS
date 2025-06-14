:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: kernel/trace/tracepoint.md

- Translation time: 2025-06-14 09:36:42

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# Tracepoints

> Author: Chen Linfeng  
>
> Email: chenlinfeng25@outlook.com

## Overview
Tracepoints are a tracing mechanism provided by the Linux kernel, allowing developers to insert probe points at specific locations in the kernel code to collect runtime information. Unlike kprobes, tracepoints are predefined and are typically used for collecting performance data or debugging information without modifying the kernel code.

## Workflow
1. **Define Tracepoint**: Kernel developers define tracepoints in the code using the macro ``define_event_trace``.
2. **Trigger Tracepoint**: Developers can trigger the defined tracepoints from other parts of the code by using the functions defined with ``define_event_trace``, and pass relevant context information. For example, if the defined tracepoint is named ``my_tracepoint``, it can be triggered using ``trace_my_tracepoint()``.
3. **Collect Data**: When a tracepoint is triggered, the kernel records the related data. This data can be analyzed using user-space tools (such as ``trace-cmd`` or ``perf``). Currently, DragonOS only supports viewing the data files created by the kernel for these data.
    1. Reading the ``/sys/kernel/debug/tracing/trace`` file can view the tracepoint collected data. The buffer content will not be cleared.
    2. Reading the ``/sys/kernel/debug/tracing/trace_pipe`` file can view the tracepoint collected data. The buffer content will be cleared. If there is no content in the buffer, it will block until new data is available.
    3. Reading the ``/sys/kernel/debug/tracing/events/`` directory can view all available tracepoints.
    4. Reading the ``/sys/kernel/debug/tracing/events/<event_name>/format`` file can view the data format of a specific tracepoint.
    5. Writing to the ``/sys/kernel/debug/tracing/events/<event_name>/enable`` file can enable a specific tracepoint.
    6. Writing an empty value to ``/sys/kernel/debug/tracing/trace`` can clear the current trace data.
4. **Analyze Data**: User-space tools can read the data collected by tracepoints and generate reports or charts to help developers understand kernel behavior.

## Interface
```rust
define_event_trace!() // 定义一个 tracepoint
```

```rust
// example
define_event_trace!(
    sys_enter_openat,
    TP_system(syscalls),
    TP_PROTO(dfd: i32, path:*const u8, o_flags: u32, mode: u32),
    TP_STRUCT__entry{
        dfd: i32,
        path: u64,
        o_flags: u32,
        mode: u32,
    },
    TP_fast_assign{
        dfd: dfd,
        path: path as u64,
        o_flags: o_flags,
        mode: mode,
    },
    TP_ident(__entry),
    TP_printk({
        format!(
            "dfd: {}, path: {:#x}, o_flags: {:?}, mode: {:?}",
            __entry.dfd,
            __entry.path,
            __entry.o_flags,
            __entry.mode
        )
    })
);
```

## Tracepoints For eBPF
In DragonOS, tracepoints can also be used in conjunction with eBPF to collect richer data within the kernel. eBPF programs can be attached to tracepoints to execute custom logic when a tracepoint is triggered.
