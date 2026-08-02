#![no_std]
//! 用户态断点探针（uprobe）支持——架构无关核心与 x86_64 指令分析。
//!
//! 与 kprobe 的关键差异：被探测指令位于**用户地址空间**，单步执行原指令必须借助
//! XOL（eXecute Out of Line，在用户态 slot 页执行副本），不能像 kprobe 那样把 rip
//! 指向内核缓冲区（CPL=3 时内核页 supervisor-only + NX 不可执行）。因此本 crate：
//! - 只保存原指令副本与 XOL slot 偏移，**不**提供任何内核态“单步地址”；
//! - 指令分析直接复用 yaxpeax-x86（kprobe 已依赖）。
//!
//! 本 crate 是 uprobe 整体实现的第一批（计划步骤 1+2），仅含 crate 内部数据结构、
//! trait 与 x86 指令分析；mm 集成 / 异常分发 / perf 接入由后续步骤完成。

extern crate alloc;

pub mod arch;
pub mod core;

pub use crate::core::*;
pub use arch::*;
