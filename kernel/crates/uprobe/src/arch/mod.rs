//! uprobe 的架构无关 trait 定义与架构分发。
//!
//! - [`ProbeArgs`]：处理器寄存器/陷阱帧的架构无关视图（供回调使用）。
//! - [`CallBackFunc`]：事件回调（典型为 eBPF 程序入口）。

use ::core::any::Any;

#[cfg(target_arch = "x86_64")]
mod x86;

#[cfg(target_arch = "x86_64")]
pub use x86::*;

/// 处理器寄存器/陷阱帧的架构无关视图（供回调使用）。
///
/// 与 kprobe 的 `ProbeArgs` 保持相同签名，以便复用同一套 TrapFrame 适配模式；
/// 但 uprobe 是独立 crate，不依赖 kprobe crate（低耦合），故单独定义。
pub trait ProbeArgs: Send {
    /// 供使用者转换为特定架构的 TrapFrame。
    fn as_any(&self) -> &dyn Any;
    /// 触发断点异常（#BP）的指令地址（对 uprobe 即 probe_vaddr）。
    fn break_address(&self) -> usize;
    /// 触发单步异常（#DB）的指令地址（XOL slot 中原指令执行后的下一条）。
    fn debug_address(&self) -> usize;
}

/// 事件回调（典型为 eBPF 程序入口）。
///
/// 与 kprobe 的 `CallBackFunc` 同签名。使用 `Arc` 以便在多个 per-mm 探测点间共享
/// 同一回调实例。
pub trait CallBackFunc: Send + Sync {
    fn call(&self, trap_frame: &dyn ProbeArgs);
}
