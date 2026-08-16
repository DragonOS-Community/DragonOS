//! uprobe 的架构无关 trait 定义与架构分发。
//!
//! - [`ProbeArgs`]：处理器寄存器/陷阱帧的架构无关视图（供回调使用）。
//! - [`UprobeOps`]：探测点的访问接口（镜像 kprobe 的 `KprobeOps`，但面向用户态，
//!   **不**提供返回内核缓冲区的 `single_step_address`）。
//! - [`CallBackFunc`]：事件回调（典型为 eBPF 程序入口）。

use ::core::any::Any;

#[cfg(target_arch = "x86_64")]
mod x86;

#[cfg(target_arch = "x86_64")]
pub use x86::*;

use crate::core::UprobePoint;

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

/// uprobe 探测点的访问接口。
///
/// **关键差异（相对 kprobe 的 `KprobeOps`）**：不提供 `single_step_address`——uprobe
/// 的单步地址是 per-mm 的 XOL slot 用户地址，需 mm 上下文在运行时计算（计划步骤
/// 3/5），不属于本 crate。分发器 / handler 通过本 trait 即可取得「原指令副本 + 长度
/// + XOL slot 偏移」，XOL slot 的真实用户地址由 mm 层另行提供。
pub trait UprobeOps: Send {
    /// 0xcc 断点安装地址（即 probe_vaddr）。
    fn break_address(&self) -> usize;
    /// 原指令执行完毕后应恢复执行的地址（= break_address + insn_len）。
    fn return_address(&self) -> usize;
    /// 原指令副本（前 [`UprobeOps::insn_len`] 字节有效）。
    fn old_instruction(&self) -> &[u8];
    /// 原指令解码长度。
    fn insn_len(&self) -> usize;
    /// XOL slot 在 per-mm XOL 页内的偏移（mm 层填充）。
    fn xol_slot_offset(&self) -> usize;
}

impl UprobeOps for UprobePoint {
    fn break_address(&self) -> usize {
        self.probe_vaddr
    }
    fn return_address(&self) -> usize {
        self.probe_vaddr + self.insn_len
    }
    fn old_instruction(&self) -> &[u8] {
        &self.old_instruction[..self.insn_len]
    }
    fn insn_len(&self) -> usize {
        self.insn_len
    }
    fn xol_slot_offset(&self) -> usize {
        self.xol_slot_offset
    }
}

/// 处理器函数指针类型。
pub type HandlerFn = fn(&dyn ProbeArgs);

/// 函数指针形式的（pre/post）处理器包装。
pub(crate) struct ProbeHandler {
    func: fn(&dyn ProbeArgs),
}

impl ProbeHandler {
    pub fn new(func: fn(&dyn ProbeArgs)) -> Self {
        ProbeHandler { func }
    }
    /// 调用处理器。
    pub fn call(&self, trap_frame: &dyn ProbeArgs) {
        (self.func)(trap_frame);
    }

    /// 包装的函数指针（供 fork 继承等场景读取）。
    pub fn func(&self) -> fn(&dyn ProbeArgs) {
        self.func
    }
}

/// 事件回调（典型为 eBPF 程序入口）。
///
/// 与 kprobe 的 `CallBackFunc` 同签名。使用 `Arc` 以便在多个 per-mm 探测点间共享
/// 同一回调实例。
pub trait CallBackFunc: Send + Sync {
    fn call(&self, trap_frame: &dyn ProbeArgs);
}
