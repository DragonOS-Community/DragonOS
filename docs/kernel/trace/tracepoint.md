# Tracepoints

> 作者: 陈林峰
>
> Email: chenlinfeng25@outlook.com


## 概述
Tracepoints 是 Linux 内核提供的一种跟踪机制，允许开发者在内核代码的特定位置插入探测点，以便收集运行时信息。与 kprobes 不同，tracepoints 是预定义的，并且通常用于收集性能数据或调试信息，而不需要修改内核代码。

## 工作流程
1. **定义 Tracepoint**: 内核开发者在代码中定义 tracepoint，使用宏 `define_event_trace`。
2. **触发 Tracepoint**: 在代码的其他部分，开发者可以使用 `define_event_trace` 定义的函数来触发已定义的 tracepoint，并传递相关的上下文信息。比如定义的tracepoint名称为`my_tracepoint`,则可以使用 `trace_my_tracepoint()` 来触发它。
3.  **收集数据**: 当 tracepoint 被触发时，内核会记录相关的数据，这些数据可以通过用户空间工具（如 `trace-cmd` 或 `perf`）进行分析。现在DragonOS中还只能支持查看内核为这些数据创建的文件。
    1.  读取`/sys/kernel/debug/tracing/trace`文件可以查看tracepoint收集的数据。缓冲区的内容不会被清除。
    2.  读取`/sys/kernel/debug/tracing/trace_pipe`文件可以查看tracepoint收集的数据。缓冲区的内容会被清除。如果缓冲区没有内容，则会阻塞等待新的数据。
    3.  读取`/sys/kernel/debug/tracing/events/`目录可以查看所有可用的 tracepoints。
    4.  读取`/sys/kernel/debug/tracing/events/<event_name>/format`文件可以查看特定 tracepoint 的数据格式。
    5.  写入`/sys/kernel/debug/tracing/events/<event_name>/enable` 文件可以启用特定的 tracepoint。
    6.  写入空值到 `/sys/kernel/debug/tracing/trace` 可以清空当前的 trace 数据。
4. **分析数据**: 用户空间工具可以读取 tracepoint 收集的数据，并生成报告或图表，以帮助开发者理解内核行为。


## 接口
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
在 DragonOS 中，tracepoints 也可以与 eBPF 结合使用，以便在内核中收集更丰富的数据。eBPF 程序可以附加到 tracepoints 上，以便在 tracepoint 被触发时执行自定义的逻辑。
