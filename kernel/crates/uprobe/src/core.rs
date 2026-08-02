//! uprobe 的架构无关核心数据结构：探测点信息、注册实体与 builder。
//!
//! 与 kprobe 的关键差异：被探测指令位于用户地址空间，单步执行原指令必须借助
//! XOL（在用户态 slot 页执行副本），因此探测点结构只保存原指令副本与 XOL slot
//! 偏移，不保存任何内核态“单步地址”。原指令的读取与分析由调用方（mm 层 / perf
//! 层）借助本 crate 导出的 [`crate::analyze_insn`] 完成后回填入 [`UprobePoint`]。

use ::alloc::sync::Arc;

use crate::arch::{CallBackFunc, ProbeArgs, ProbeHandler};

/// 用户态指令副本的最大字节数。
///
/// x86_64 单条指令最长 15 字节；取 16 既是 2 的幂、便于对齐，也正好与一个 XOL slot
/// 的典型宽度一致（slot 需容纳指令副本并保证其后字节可安全执行/跳转）。
pub const UPROBE_INSN_COPY_SIZE: usize = 16;

/// 探测点信息（架构无关数据载体）。
///
/// `old_instruction` 的前 `insn_len` 字节为有效原指令副本；其余字节填充 0。
/// `xol_slot_offset` 为该探测点在所属 per-mm XOL 页内的偏移，由 mm 层（计划步骤 3）
/// 在分配 slot 时填充，本 crate 仅占位（初始 0）。
#[derive(Debug)]
pub struct UprobePoint {
    /// 被探测的用户态虚拟地址（即 0xcc 断点安装地址）。
    pub probe_vaddr: usize,
    /// 原指令副本（前 `insn_len` 字节有效）。
    pub old_instruction: [u8; UPROBE_INSN_COPY_SIZE],
    /// 原指令解码长度（1..=15）。
    pub insn_len: usize,
    /// XOL slot 在 per-mm XOL 页内的偏移（mm 层填充，初始为 0）。
    pub xol_slot_offset: usize,
}

impl UprobePoint {
    /// 以给定探测地址创建一个空白探测点：原指令副本与长度待指令分析后填充，
    /// XOL slot 偏移待 mm 层分配 slot 时填充。
    pub fn new(probe_vaddr: usize) -> Self {
        UprobePoint {
            probe_vaddr,
            old_instruction: [0u8; UPROBE_INSN_COPY_SIZE],
            insn_len: 0,
            xol_slot_offset: 0,
        }
    }
}

/// 注册后的 uprobe 实体：探测点 + 回调 + 使能标志。
pub struct UprobeBasic {
    probe_vaddr: usize,
    pre_handler: ProbeHandler,
    post_handler: ProbeHandler,
    event_callback: Option<Arc<dyn CallBackFunc>>,
    probe_point: Option<Arc<UprobePoint>>,
    enable: bool,
}

impl UprobeBasic {
    /// 调用前置处理器（#BP 命中、BPF 入口前）。
    pub fn call_pre_handler(&self, trap_frame: &dyn ProbeArgs) {
        self.pre_handler.call(trap_frame);
    }

    /// 调用后置处理器（XOL 单步完成、返回原址前）。
    pub fn call_post_handler(&self, trap_frame: &dyn ProbeArgs) {
        self.post_handler.call(trap_frame);
    }

    /// 调用事件回调（典型为 eBPF 程序入口）。
    pub fn call_event_callback(&self, trap_frame: &dyn ProbeArgs) {
        if let Some(callback) = &self.event_callback {
            callback.call(trap_frame);
        }
    }

    /// 更新事件回调。
    pub fn update_event_callback(&mut self, callback: Arc<dyn CallBackFunc>) {
        self.event_callback = Some(callback);
    }

    pub fn disable(&mut self) {
        self.enable = false;
    }

    pub fn enable(&mut self) {
        self.enable = true;
    }

    pub fn is_enabled(&self) -> bool {
        self.enable
    }

    /// 被探测的用户态虚拟地址。
    pub fn probe_vaddr(&self) -> usize {
        self.probe_vaddr
    }

    /// 关联的探测点（若已设置）。
    pub fn probe_point(&self) -> Option<&Arc<UprobePoint>> {
        self.probe_point.as_ref()
    }
}

/// uprobe 的 builder（镜像 kprobe 的 `KprobeBuilder`，但面向用户态）。
///
/// uprobe 探测对象是用户态地址，本 crate 无法直接读取用户内存（需目标 mm 的页表
/// 上下文），因此 builder 只持有“已解析的用户虚拟地址”与回调：
/// - `path` + `offset` 到 `probe_vaddr` 的解析属于 perf 层（计划步骤 7）职责；
/// - 原指令的读取与分析由调用方借助 [`crate::analyze_insn`] 完成后回填入
///   [`UprobePoint`]（可通过 [`UprobeBuilder::with_probe_point`] 注入）。
pub struct UprobeBuilder {
    probe_vaddr: usize,
    pre_handler: ProbeHandler,
    post_handler: ProbeHandler,
    event_callback: Option<Arc<dyn CallBackFunc>>,
    probe_point: Option<Arc<UprobePoint>>,
    enable: bool,
}

impl UprobeBuilder {
    pub fn new(
        probe_vaddr: usize,
        pre_handler: fn(&dyn ProbeArgs),
        post_handler: fn(&dyn ProbeArgs),
        enable: bool,
    ) -> Self {
        UprobeBuilder {
            probe_vaddr,
            pre_handler: ProbeHandler::new(pre_handler),
            post_handler: ProbeHandler::new(post_handler),
            event_callback: None,
            probe_point: None,
            enable,
        }
    }

    pub fn with_event_callback(mut self, event_callback: Arc<dyn CallBackFunc>) -> Self {
        self.event_callback = Some(event_callback);
        self
    }

    pub fn with_probe_point(mut self, probe_point: Arc<UprobePoint>) -> Self {
        self.probe_point = Some(probe_point);
        self
    }

    /// 消费 builder，构造注册实体。
    ///
    /// 若未显式提供 `probe_point`，则以 `probe_vaddr` 创建一个空白探测点（原指令
    /// 副本 / insn_len / XOL slot 偏移待后续填充）。
    pub fn build(self) -> UprobeBasic {
        let probe_point = self
            .probe_point
            .unwrap_or_else(|| Arc::new(UprobePoint::new(self.probe_vaddr)));
        UprobeBasic {
            probe_vaddr: self.probe_vaddr,
            pre_handler: self.pre_handler,
            post_handler: self.post_handler,
            event_callback: self.event_callback,
            probe_point: Some(probe_point),
            enable: self.enable,
        }
    }
}
