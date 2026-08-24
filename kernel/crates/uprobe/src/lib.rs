#![no_std]
//! 用户态断点探针（uprobe）支持——架构无关核心与 x86_64 指令分析。
//!
//! 与 kprobe 的关键差异：被探测指令位于**用户地址空间**，单步执行原指令必须借助
//! XOL（eXecute Out of Line，在用户态 slot 页执行副本），不能像 kprobe 那样把 rip
//! 指向内核缓冲区（CPL=3 时内核页 supervisor-only + NX 不可执行）。因此本 crate：
//! - 只保存原指令副本与 XOL slot 偏移，**不**提供任何内核态“单步地址”；
//! - 指令分析直接复用 yaxpeax-x86（kprobe 已依赖）。
//!
//! 本 crate 仅保留纯指令分析/重定位与命中回调的架构无关接口；MM、异常和 perf
//! 生命周期由内核对应模块拥有。

/// 用户态指令副本的最大字节数。x86-64 指令最长 15 字节，16 字节同时作为 XOL
/// slot 宽度，便于固定大小复制与对齐。
pub const UPROBE_INSN_COPY_SIZE: usize = 16;

pub mod arch;

pub use arch::*;
