//! Per-mm uprobe 管理 + XOL 区 + 断点页安装（计划步骤 3+4）。
//!
//! 本模块提供 uprobe 注册/注销的基础设施，供 batch3（异常分发）与 batch4（perf 接入）调用。
//!
//! # 关键设计（评审 findings）
//!
//! - **F8**：`uprobe_list` / `xol_area` / `uprobe_page_state` 挂在 `AddressSpace` 上，由
//!   **独立 irqsave `SpinLock`** 保护（**不**走 `inner: RwSem`），命中路径（#BP/#DB 关中断）
//!   仅 `lock_irqsave` + 查表，绝不睡眠。
//! - **F1/F2**：断点页安装复刻 `do_wp_page` 私有文件 COW——`copy_page_as_normal` + patch
//!   0xcc + **单次** `set_entry` 原子帧替换（**绝不** unmap+map_phys 制造瞬时空 PTE）+
//!   `insert_vma`/`remove_vma` rmap 账簿 + `flush_tlb_range`。
//! - **F7**：每目标 mm 私有 COW 副本（type `Normal`），**绝不**修改共享 page-cache 页
//!   （否则 writeback 回写 0xcc 损坏 .so）。
//! - **F6 装弹顺序不变量**：注册时严格按 XOL slot 分配 → uprobe 表项插入 → 0xcc 页发布；
//!   0xcc 发布前任何路径查该 vaddr 必须能找到就绪 uprobe 表项。
#![allow(dead_code)] // 公开 API 供 batch3/batch4 调用，当前批次尚未引用

use crate::libs::spinlock::SpinLock;
use alloc::{
    boxed::Box,
    collections::BTreeMap,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicU64, Ordering};

use system_error::SystemError;

use crate::{
    arch::{mm::PageMapper, MMArch},
    libs::rwlock::RwLock,
    mm::{
        page::{page_manager_lock, Page, PageEntry},
        syscall::{MapFlags, ProtFlags},
        MemoryManagementArch, PhysAddr, VirtAddr, VmFlags,
    },
};

use super::RwSemWriteGuard;
use super::{AddressSpace, InnerAddressSpace, LockedVMA};

use uprobe::{
    analyze_insn, build_xol_slot, InsnAnalysis, ProbeArgs, UprobeBasic, UprobeBuilder, UprobePoint,
    UPROBE_INSN_COPY_SIZE,
};

// ──────────────────────────── XOL 区 ────────────────────────────

/// 每个 slot 的宽度（= `UPROBE_INSN_COPY_SIZE` = 16 字节）。
const XOL_SLOT_SIZE: usize = UPROBE_INSN_COPY_SIZE;

/// 每页 slot 数量（4096 / 16 = 256）。
const XOL_SLOTS_PER_PAGE: usize = MMArch::PAGE_SIZE / XOL_SLOT_SIZE;

/// slot 位图需要的 u64 字数（256 bits → 4 words）。
const XOL_BITMAP_WORDS: usize = XOL_SLOTS_PER_PAGE.div_ceil(64);

/// Per-mm XOL（eXecute Out of Line）区。
///
/// 在用户地址空间映射一个可读可执行页，分成 16 字节对齐的 slot。每个 uprobe 分配一个 slot，
/// 命中时 batch3 在 slot 中写入原指令副本（RIP-relative 重定位后），rip 指向 slot 执行。
///
/// XOL 页在**注册时**（进程上下文、开中断）创建，**不能**在命中路径（关中断）创建。
pub struct XolArea {
    /// XOL 页在用户空间的基地址。
    page_base: VirtAddr,
    /// XOL 页的物理地址（供 batch3 在关中断路径下通过 `phys_2_virt` 直接写 slot 内容，
    /// 无需 mapper / RwSem）。
    page_paddr: PhysAddr,
    /// slot 分配位图（bit=1 表示已占用）。
    slot_bitmap: [u64; XOL_BITMAP_WORDS],
}

impl XolArea {
    fn alloc_slot(&mut self) -> Option<usize> {
        for (word_idx, word) in self.slot_bitmap.iter_mut().enumerate() {
            if *word != u64::MAX {
                let bit = (!*word).trailing_zeros() as usize;
                let slot = word_idx * 64 + bit;
                if slot >= XOL_SLOTS_PER_PAGE {
                    break;
                }
                *word |= 1u64 << bit;
                return Some(slot * XOL_SLOT_SIZE);
            }
        }
        None
    }

    fn free_slot(&mut self, offset: usize) {
        let slot = offset / XOL_SLOT_SIZE;
        if slot < XOL_SLOTS_PER_PAGE {
            self.slot_bitmap[slot / 64] &= !(1u64 << (slot % 64));
        }
    }

    /// 计算 slot 对应的用户虚拟地址（供 batch3 使用）。
    pub fn slot_vaddr(&self, offset: usize) -> VirtAddr {
        VirtAddr::new(self.page_base.data() + offset)
    }

    /// XOL 页基地址（供 batch3 计算 slot 地址）。
    pub fn page_base(&self) -> VirtAddr {
        self.page_base
    }

    /// XOL 页物理地址（供 batch3 在关中断路径下通过 `phys_2_virt` 写 slot 内容）。
    pub fn page_paddr(&self) -> PhysAddr {
        self.page_paddr
    }
}

impl core::fmt::Debug for XolArea {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("XolArea")
            .field("page_base", &self.page_base)
            .finish_non_exhaustive()
    }
}

// ──────────────────────── per-mm uprobe 实例 ────────────────────────

/// Per-mm uprobe 实例：注册实体 + 指令分析结果。
///
/// 存储在 `AddressSpace::uprobe_list` 中，以 `probe_vaddr` 为键。同一地址可有多个实例
/// （`Vec<Arc<RwLock<UprobeInstance>>>`，镜像 kprobe 的 `break_list`）。
///
/// batch3 命中路径用法：
/// ```ignore
/// let list = mm.uprobe_list.lock_irqsave();
/// if let Some(entries) = list.get(&probe_vaddr) {
///     for entry in entries {
///         let inst = entry.read();
///         if inst.basic.is_enabled() {
///             inst.basic.call_pre_handler(args);
///             inst.basic.call_event_callback(args);
///             // 取 xol_slot_offset、insn_analysis 做 XOL slot 填充 …
///         }
///     }
/// }
/// ```
pub struct UprobeInstance {
    /// uprobe 实体（探测点 + 处理器 + 回调）。
    pub basic: UprobeBasic,
    /// x86 指令静态分析（命中时供 `build_xol_slot` 用）。
    pub insn_analysis: InsnAnalysis,
    /// 拥有此实例的消费者（perf event fd）id（评审 R9）。
    /// fork 继承的子实例沿用父实例的 id，使消费者 close 时一并注销。
    pub consumer_id: u64,
}

impl core::fmt::Debug for UprobeInstance {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UprobeInstance")
            .field("probe_vaddr", &self.basic.probe_vaddr())
            .field("insn_analysis", &self.insn_analysis)
            .finish()
    }
}
/// 某个页上已安装断点的状态标记。
///
/// 以页基地址（`probe_vaddr & !(PAGE_SIZE-1)`）为键。多个 uprobe 命中同一页时共享
/// 一个 COW 副本，`refcount` 记录活跃断点数。注销在**当前映射页**上恢复字节、
/// 不换页（评审 R8），故此处仅保留计数标记（供安装路径判定「页已私有化」）。
pub(crate) struct UprobePageState {
    /// 活跃断点数。
    refcount: usize,
}

impl core::fmt::Debug for UprobePageState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UprobePageState")
            .field("refcount", &self.refcount)
            .finish_non_exhaustive()
    }
}
// ──────────────────────── 注册句柄 ────────────────────────

/// 已注册 uprobe 的句柄。
///
/// - `Drop` 时自动注销（供 batch4 的 `UprobePerfEvent::drop` 使用，参照
///   `KprobePerfEvent::drop → unregister_kprobe`）。
/// - 也可显式调用 [`uprobe_unregister`] 提前注销。
/// - 通过 [`UprobeHandle::instance`] 可访问 `Arc<RwLock<UprobeInstance>>`，供 batch4
///   注入 `event_callback`（`instance().write().basic.update_event_callback(arc)`）。
pub struct UprobeHandle {
    mm: Weak<AddressSpace>,
    probe_vaddr: usize,
    entry: Option<Arc<RwLock<UprobeInstance>>>,
}

impl UprobeHandle {
    /// 访问底层实例（供 batch4 注入 BPF 回调）。
    pub fn instance(&self) -> Option<&Arc<RwLock<UprobeInstance>>> {
        self.entry.as_ref()
    }

    /// 被探测的用户虚拟地址。
    pub fn probe_vaddr(&self) -> usize {
        self.probe_vaddr
    }
}

impl Drop for UprobeHandle {
    fn drop(&mut self) {
        if let Some(entry) = self.entry.take() {
            if let Some(mm) = self.mm.upgrade() {
                uprobe_unregister_internal(&mm, self.probe_vaddr, &entry);
            }
        }
    }
}

// ──────────────────────── 公开 API ────────────────────────

/// # 注册一个 uprobe
///
/// 在目标 mm 的 `probe_vaddr` 处安装 0xcc 断点。注册流程（装弹顺序 F6）：
/// 1. 查 `uprobe_list` 是否已有同址条目：有则复用其 old_instruction + insn_analysis
///    （避免读到 COW 副本里的 0xcc）；无则读原指令 + `analyze_insn` 校验；
/// 2. 分配 XOL slot（填 `xol_slot_offset`）；
/// 3. 用真实 slot_vaddr 调 `build_xol_slot` 预填 slot + 校验 RIP-relative 位移
///    （溢出→EINVAL fail-fast，绝不留下命中时 panic 的探针）；
/// 4. 插入 `uprobe_list` 表项；
/// 5. 安装 0xcc（私有 COW，复刻 `do_wp_page`）。
///
/// ## 参数
/// - `mm`：目标地址空间。
/// - `probe_vaddr`：被探测的用户虚拟地址（必须在已映射的可执行 VMA 内且页已 present）。
/// - `pre_handler`：#BP 命中前置处理器（batch3 提供，或用 [`noop_handler`] 占位）。
/// - `post_handler`：XOL 单步完成后置处理器。
///
/// ## 返回
/// `Ok(UprobeHandle)` 或错误码（`EINVAL`=地址非法/指令不支持，`EFAULT`=页未映射，
/// `ENOMEM`=内存不足/XOL 区满，`EACCES`=VMA 不可执行）。
///
pub fn uprobe_register(
    mm: &Arc<AddressSpace>,
    probe_vaddr: usize,
    pre_handler: fn(&dyn ProbeArgs),
    post_handler: fn(&dyn ProbeArgs),
    consumer_id: u64,
) -> Result<UprobeHandle, SystemError> {
    // ── 持有 inner.write() 整个注册过程 ──
    let mut inner = mm.write();

    // ── Step 1: 定位 VMA + 读原指令 + 分析 ──
    let vaddr = VirtAddr::new(probe_vaddr);
    let page_base_addr = probe_vaddr & !(MMArch::PAGE_SIZE - 1);
    let page_offset = probe_vaddr & (MMArch::PAGE_SIZE - 1);

    // VMA 必须存在且可执行
    let vma = inner.mappings.contains(vaddr).ok_or(SystemError::EINVAL)?;
    {
        let vm_flags = *vma.lock().vm_flags();
        if !vm_flags.contains(VmFlags::VM_EXEC | VmFlags::VM_MAYEXEC) {
            return Err(SystemError::EACCES);
        }
    }

    // ── P1：重复注册同一 probe_vaddr 时复用已有指令信息（避免读到 0xcc）──
    // 第二个 consumer 注册同一地址时 PTE 已指向含 0xcc 的 COW 副本，
    // read_user_insn_bytes 会把 0xcc 当原指令。故先查 uprobe_list：若有同址条目，
    // 复用其 old_instruction + insn_analysis（二者对所有同址实例一致），跳过读取。
    let reused = {
        let list = mm.uprobe_list.lock_irqsave();
        list.get(&probe_vaddr)
            .and_then(|entries| entries.first())
            .and_then(|entry| {
                let inst = entry.read();
                inst.basic
                    .probe_point()
                    .map(|pp| (pp.old_instruction, inst.insn_analysis))
            })
    };

    let (old_instruction, analysis) = if let Some((oi, an)) = reused {
        (oi, an)
    } else {
        // 无已有条目——正常读取 + 分析。
        let insn_bytes = read_user_insn_bytes(&inner.user_mapper.utable, probe_vaddr)?;
        let analysis = analyze_insn(insn_bytes.as_slice()).map_err(|e| {
            log::warn!(
                "uprobe_register: analyze_insn failed at {:#x}: {:?}",
                probe_vaddr,
                e
            );
            SystemError::EINVAL
        })?;
        // 评审 R6：read_user_insn_bytes 已跨页补全真实字节（非零填充），
        // 这里必须拷贝**整条**指令（insn_len ≤ 15 < 16），而不是仅本页剩余
        // 字节——否则 XOL 副本尾部是零，执行为另一条指令。
        let mut old_instruction = [0u8; UPROBE_INSN_COPY_SIZE];
        old_instruction[..analysis.insn_len].copy_from_slice(&insn_bytes[..analysis.insn_len]);
        (old_instruction, analysis)
    };

    // ── Step 2: 确保 XOL 区存在 + 分配 slot ──
    let xol_slot_offset = ensure_xol_and_alloc_slot(mm, &mut inner)?;

    // ── P2：注册时预填 XOL slot + 验证 RIP-relative 位移（fail-fast）──
    // slot_vaddr = xol_page_base + slot_offset 此时已知，立即用真实地址调
    // build_xol_slot：位移溢出→EINVAL（注册失败，不引入会在命中时 panic 的探针）；
    // 成功→slot 内容写入物理页，命中时（#BP handler）slot 已就绪、直接 rip→slot。
    {
        let (slot_vaddr, page_paddr) = {
            let guard = mm.xol_area.lock_irqsave();
            let area = guard.as_ref().ok_or(SystemError::EFAULT)?;
            (area.slot_vaddr(xol_slot_offset), area.page_paddr())
        };

        let mut slot_buf = [0u8; UPROBE_INSN_COPY_SIZE];
        if let Err(e) = build_xol_slot(
            &analysis,
            probe_vaddr,
            slot_vaddr.data(),
            &old_instruction,
            &mut slot_buf,
        ) {
            log::warn!(
                "uprobe_register: build_xol_slot failed at {:#x} (slot {:#x}): {:?}",
                probe_vaddr,
                slot_vaddr.data(),
                e
            );
            free_xol_slot(mm, xol_slot_offset);
            return Err(SystemError::EINVAL);
        }

        // 写入 XOL slot 物理页（复刻 batch3 fill_xol_slot / patch_byte_in_phys 写法）。
        let kva = unsafe { MMArch::phys_2_virt(page_paddr) }.ok_or_else(|| {
            free_xol_slot(mm, xol_slot_offset);
            SystemError::EFAULT
        })?;
        unsafe {
            let dst = (kva.data() + xol_slot_offset) as *mut u8;
            core::ptr::copy_nonoverlapping(slot_buf.as_ptr(), dst, UPROBE_INSN_COPY_SIZE);
        }
    }

    // ── Step 3: 创建 uprobe 实体 ──
    let mut point = UprobePoint::new(probe_vaddr);
    point.old_instruction = old_instruction;
    point.insn_len = analysis.insn_len;
    point.xol_slot_offset = xol_slot_offset;

    let basic = UprobeBuilder::new(probe_vaddr, pre_handler, post_handler, true)
        .with_probe_point(Arc::new(point))
        .build();

    let entry = Arc::new(RwLock::new(UprobeInstance {
        basic,
        insn_analysis: analysis,
        consumer_id,
    }));

    // ── Step 4: 插入 uprobe_list（表项在 0xcc 发布前就绪 — F6）──
    {
        let mut list = mm.uprobe_list.lock_irqsave();
        list.entry(probe_vaddr).or_default().push(entry.clone());
    }

    // ── Step 5: 安装 0xcc 断点页 ──
    if let Err(e) = install_breakpoint_page(mm, &mut inner, &vma, page_base_addr, page_offset) {
        // 回滚：移除表项 + 释放 slot
        {
            let mut list = mm.uprobe_list.lock_irqsave();
            if let Some(entries) = list.get_mut(&probe_vaddr) {
                entries.retain(|x| !Arc::ptr_eq(x, &entry));
            }
        }
        free_xol_slot(mm, xol_slot_offset);
        return Err(e);
    }

    drop(inner);

    Ok(UprobeHandle {
        mm: Arc::downgrade(mm),
        probe_vaddr,
        entry: Some(entry),
    })
}

/// # 注销一个 uprobe
///
/// 消费句柄并执行注销（反序：恢复原页 → 移除表项 → 回收 slot）。
/// 也可直接 drop `UprobeHandle`，效果相同。
pub fn uprobe_unregister(mut handle: UprobeHandle) {
    if let Some(entry) = handle.entry.take() {
        if let Some(mm) = handle.mm.upgrade() {
            uprobe_unregister_internal(&mm, handle.probe_vaddr, &entry);
        }
    }
}

// ──────────────────────── 内部实现 ────────────────────────

/// 从目标 mm 的页表读取 probe_vaddr 处的指令字节（最多 16 字节）。
///
/// `PageMapper::translate` 直接 walk 物理页表，不需要目标 mm 的 CR3 上下文，
/// 因此可跨进程读取。若页未 present 返回 `EFAULT`。
fn read_user_insn_bytes(
    mapper: &PageMapper,
    probe_vaddr: usize,
) -> Result<[u8; UPROBE_INSN_COPY_SIZE], SystemError> {
    let page_offset = probe_vaddr & (MMArch::PAGE_SIZE - 1);
    let mut bytes = [0u8; UPROBE_INSN_COPY_SIZE];

    // 第一页：从 probe_vaddr 读到页末（至多 16 字节）
    let (paddr, _flags) = mapper
        .translate(VirtAddr::new(probe_vaddr))
        .ok_or(SystemError::EFAULT)?;
    let kva = unsafe { MMArch::phys_2_virt(paddr) }.ok_or(SystemError::EFAULT)?;
    let avail = MMArch::PAGE_SIZE - page_offset;
    let read_len = avail.min(UPROBE_INSN_COPY_SIZE);
    unsafe {
        core::ptr::copy_nonoverlapping(
            (kva.data() + page_offset) as *const u8,
            bytes.as_mut_ptr(),
            read_len,
        );
    }

    // 跨页：若本页剩余字节不足 16，继续从下一页读取补全。
    // 否则零填充会使解码器用伪造的零字节成功解码（如页末 call rel32），
    // 导致 XOL 执行与真实下一条指令不同的代码。
    if read_len < UPROBE_INSN_COPY_SIZE {
        let next_vaddr = probe_vaddr + avail; // 下一页起始（页对齐）
        if let Some((next_paddr, _)) = mapper.translate(VirtAddr::new(next_vaddr)) {
            if let Some(next_kva) = unsafe { MMArch::phys_2_virt(next_paddr) } {
                let remain = UPROBE_INSN_COPY_SIZE - read_len;
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        next_kva.data() as *const u8,
                        bytes.as_mut_ptr().add(read_len),
                        remain,
                    );
                }
            }
            // 若下一页无 direct-map（理论罕见），剩余字节保持零填充，
            // 解码器对跨页指令会返回 Truncated（bytes.len < insn_len 在
            // analyze_insn 内仅当解码需要跨零字节边界时才可能误判；
            // 但函数入口通常不在页末，且零填充比读错更安全）。
        }
        // 若下一页未映射，剩余为零——解码器会因非法字节失败，注册被拒绝（安全）。
    }

    Ok(bytes)
}

/// 确保 mm 有 XOL 区，并分配一个 slot，返回 slot 在页内偏移。
fn ensure_xol_and_alloc_slot(
    mm: &Arc<AddressSpace>,
    inner: &mut RwSemWriteGuard<'_, InnerAddressSpace>,
) -> Result<usize, SystemError> {
    // 快速路径：XOL 已存在 → 直接分配
    {
        let mut guard = mm.xol_area.lock_irqsave();
        if let Some(area) = guard.as_mut() {
            return area.alloc_slot().ok_or(SystemError::ENOMEM);
        }
    }

    // 慢速路径：创建 XOL 页（匿名映射，R-X）
    // map_anonymous 可能分配物理页（睡眠安全：此时未持有任何 SpinLock）
    let prot = ProtFlags::PROT_READ | ProtFlags::PROT_EXEC;
    let map_flags = MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS;
    let page = inner.map_anonymous(
        VirtAddr::new(0), // 让内核选择地址
        MMArch::PAGE_SIZE,
        prot,
        map_flags,
        true, // round_to_min
        true, // allocate_at_once（立即分配零页）
    )?;

    // 获取 XOL 页物理地址（供 batch3 关中断路径写 slot 内容）
    let page_paddr = inner
        .user_mapper
        .utable
        .translate(page.virt_address())
        .map(|(pa, _)| pa)
        .ok_or(SystemError::EFAULT)?;

    let mut area = Box::new(XolArea {
        page_base: page.virt_address(),
        page_paddr,
        slot_bitmap: [0u64; XOL_BITMAP_WORDS],
    });
    let offset = area.alloc_slot().ok_or(SystemError::ENOMEM)?;

    let mut guard = mm.xol_area.lock_irqsave();
    if guard.is_none() {
        *guard = Some(area);
    } else {
        // 因调用方持有 inner.write()，同 mm 的注册是串行的，此分支理论上不可达。
        // TODO: [stage2] unmap 冗余的 XOL 页避免泄漏
        return guard
            .as_mut()
            .unwrap()
            .alloc_slot()
            .ok_or(SystemError::ENOMEM);
    }
    Ok(offset)
}

/// 释放 XOL slot。
fn free_xol_slot(mm: &Arc<AddressSpace>, offset: usize) {
    let mut guard = mm.xol_area.lock_irqsave();
    if let Some(area) = guard.as_mut() {
        area.free_slot(offset);
    }
}

/// 安装 0xcc 断点页（复刻 do_wp_page 私有文件 COW）。
///
/// 若页已有断点（同一物理页上的另一个 uprobe），仅 patch 额外 0xcc 字节 + refcount++；
/// 否则 COW → patch → 单次 set_entry → rmap → flush_tlb_range。
fn install_breakpoint_page(
    mm: &Arc<AddressSpace>,
    inner: &mut RwSemWriteGuard<'_, InnerAddressSpace>,
    vma: &Arc<LockedVMA>,
    page_base_addr: usize,
    page_offset: usize,
) -> Result<(), SystemError> {
    let address = VirtAddr::new(page_base_addr);
    let end = VirtAddr::new(page_base_addr + MMArch::PAGE_SIZE);

    // ── 检查页是否已有断点（同页多 uprobe）──
    let already_cowed = {
        let pb = mm.uprobe_page_state.lock_irqsave();
        pb.contains_key(&page_base_addr)
    };

    if already_cowed {
        // 页已私有化：在**当前映射页** patch 额外 0xcc 字节（translate 取实时
        // paddr——写缺页二次 COW 后仍是正确页），refcount++。
        let _pt_edit = mm.page_table_edit();
        let mapper = &mut inner.user_mapper.utable;
        let (paddr, _) = mapper.translate(address).ok_or(SystemError::EFAULT)?;
        let kva = unsafe { MMArch::phys_2_virt(paddr) }.ok_or(SystemError::EFAULT)?;
        unsafe {
            core::ptr::write_volatile((kva.data() + page_offset) as *mut u8, 0xcc);
        }
        let mut pb = mm.uprobe_page_state.lock_irqsave();
        if let Some(state) = pb.get_mut(&page_base_addr) {
            state.refcount += 1;
        }
        return Ok(());
    }

    // ── 新 COW 断点页 ──

    // page_table_edit 锁（debug_assert IRQ 启用——注册在进程上下文）
    let _pt_edit = mm.page_table_edit();
    let mapper = &mut inner.user_mapper.utable;

    // translate 取旧 paddr + flags
    let (old_paddr, entry_flags) = mapper.translate(address).ok_or(SystemError::EFAULT)?;

    // 取旧 page（必须被 page_manager 追踪——File 页或 Normal 页）
    let old_page = {
        let mut pm = page_manager_lock();
        pm.get(&old_paddr).ok_or(SystemError::EFAULT)?
    };

    // COW：copy_page_as_normal → 私有 Normal 副本（type=Normal，不回写 page-cache — F7）
    let new_page = {
        let mut pm = page_manager_lock();
        pm.copy_page_as_normal(&old_paddr, mapper.allocator_mut())
            .map_err(|_| SystemError::ENOMEM)?
    };

    // patch 0xcc
    patch_byte_in_phys(&new_page, page_offset, 0xcc)?;

    // 单次原子 set_entry（绝不制造瞬时空 PTE — F1/F2）
    let table = mapper.get_table(address, 0).ok_or(SystemError::EFAULT)?;
    let i = table.index_of(address).ok_or(SystemError::EFAULT)?;
    unsafe {
        table.set_entry(i, PageEntry::new(new_page.phys_address(), entry_flags));
    }

    // mm-aware TLB shootdown
    mm.flush_tlb_range(address, end, MMArch::PAGE_SHIFT as u8, false);

    // rmap 账簿：attach 新副本，detach 旧页
    let vm_locked = vma.lock().vm_flags().contains(VmFlags::VM_LOCKED);
    new_page.write().insert_vma(vma.clone(), vm_locked);
    {
        let mut old_guard = old_page.write();
        old_guard.remove_vma(vma.as_ref());
    }
    InnerAddressSpace::remove_page_unevictable_if_unneeded(&old_page);

    // 记录页状态（页已私有化的标记）
    let mut pb = mm.uprobe_page_state.lock_irqsave();
    pb.insert(page_base_addr, UprobePageState { refcount: 1 });

    Ok(())
}

/// 在物理页的指定偏移写入一个字节（通过内核 direct-map）。
fn patch_byte_in_phys(page: &Arc<Page>, offset: usize, byte: u8) -> Result<(), SystemError> {
    let kva = unsafe { MMArch::phys_2_virt(page.phys_address()) }.ok_or(SystemError::EFAULT)?;
    unsafe {
        core::ptr::write_volatile((kva.data() + offset) as *mut u8, byte);
    }
    Ok(())
}

/// 注销内部实现（评审 R7/R8 重做）。
///
/// 顺序：移除表项 → **仅当该地址无剩余实例时**恢复断点字节 → 回收 slot →
/// 页级状态清理。
fn uprobe_unregister_internal(
    mm: &Arc<AddressSpace>,
    probe_vaddr: usize,
    entry: &Arc<RwLock<UprobeInstance>>,
) {
    // ── 1. 从 uprobe_list 移除本实例（评审 R7：同址其他 consumer 的
    //    共享断点必须保留，仅当该地址变空才恢复字节）──
    let (removed, addr_now_empty) = {
        let mut list = mm.uprobe_list.lock_irqsave();
        let mut removed = false;
        let mut now_empty = false;
        if let Some(entries) = list.get_mut(&probe_vaddr) {
            let before = entries.len();
            entries.retain(|x| !Arc::ptr_eq(x, entry));
            removed = entries.len() < before;
            if entries.is_empty() {
                list.remove(&probe_vaddr);
                now_empty = true;
            }
        }
        (removed, now_empty)
    };
    if !removed {
        return;
    }

    // 提取注销所需信息
    let (orig_first_byte, xol_slot_offset) = {
        let inst = entry.read();
        let offset = inst.basic.probe_point().map(|p| p.xol_slot_offset);
        let first_byte = inst
            .basic
            .probe_point()
            .map(|p| p.old_instruction[0])
            .unwrap_or(0);
        (first_byte, offset)
    };

    let page_base_addr = probe_vaddr & !(MMArch::PAGE_SIZE - 1);
    let page_offset = probe_vaddr & (MMArch::PAGE_SIZE - 1);

    // ── 2. 恢复断点字节（评审 R7/R8）──
    // R7：仅当该地址的最后一个实例被注销（addr_now_empty）才清除 0xcc，
    //    剩余 consumer 的探针继续命中。
    // R8：在**当前映射页**上恢复原字节，不把注册时的旧页映射回来——
    //    程序在探针活跃期间对该页其他字节的写入全部保留（私有映射语义）。
    if addr_now_empty {
        let mut inner = mm.write();
        restore_breakpoint_byte(mm, &mut inner, page_base_addr, page_offset, orig_first_byte);
    }

    // ── 3. 回收本实例的 XOL slot ──
    if let Some(offset) = xol_slot_offset {
        free_xol_slot(mm, offset);
    }

    // ── 4. 页级状态：refcount--；归零则移除标记（页保持当前映射，
    //    不回写、不换页）──
    let mut pb = mm.uprobe_page_state.lock_irqsave();
    if let Some(state) = pb.get_mut(&page_base_addr) {
        state.refcount = state.refcount.saturating_sub(1);
        if state.refcount == 0 {
            pb.remove(&page_base_addr);
        }
    }
}

/// 在**当前映射页**上恢复断点原字节（评审 R8）。
///
/// 经 `translate` 取当前 paddr（可能是断点安装时的 COW 副本，也可能是程序
/// 写缺页二次 COW 后的页），直接写回原首字节。不交换页映射——页上其他字节
/// 的任何程序写入都保留。无 PTE 变更 → 无需 TLB flush（TLB 缓存翻译而非
/// 内容；跨修改代码的串行化由 #BP 中断返回后的取指重取保证）。
fn restore_breakpoint_byte(
    mm: &Arc<AddressSpace>,
    inner: &mut RwSemWriteGuard<'_, InnerAddressSpace>,
    page_base_addr: usize,
    page_offset: usize,
    orig_first_byte: u8,
) {
    let _pt_edit = mm.page_table_edit();
    let mapper = &mut inner.user_mapper.utable;
    if let Some((paddr, _)) = mapper.translate(VirtAddr::new(page_base_addr)) {
        if let Some(kva) = unsafe { MMArch::phys_2_virt(paddr) } {
            unsafe {
                core::ptr::write_volatile((kva.data() + page_offset) as *mut u8, orig_first_byte);
            }
        }
    }
    // 页已被 munmap（translate 失败）：无需恢复。
}

// ──────────────────────── 辅助：空操作 handler ────────────────────────

/// 空操作 handler（供 batch4 在仅需 event_callback 时占位）。
pub fn noop_handler(_args: &dyn ProbeArgs) {}

// ──────────────── 全局注册表与迟到应用（评审 R9） ────────────────
//
// 注册的探针身份 = 文件 inode + 偏移（而非 open 时的映射快照）。新映射
// （dlopen/mmap）、fork 产生新地址空间时，据此把已注册的探针**迟到安装**
// 到新的 mm；exec 换新 AddressSpace，实例表自然为空（探针不跨 exec）。
//
// 消费者（perf event fd）close 时：
// 1. 从注册表移除该消费者（杜绝后续迟到安装）；
// 2. drop 其「迟到句柄」（fork/mmap 路径安装的），复用 `UprobeHandle::Drop`
//    的逐 mm 注销（含评审 R7 的地址级字节恢复）。
// 直接安装（open 时）的句柄仍由 `UprobePerfEvent::handles` 持有，drop 同理。

pub struct UprobeConsumerReg {
    pub pre_handler: fn(&dyn ProbeArgs),
    pub post_handler: fn(&dyn ProbeArgs),
    /// BPF 事件回调（可后期经 PERF_EVENT_IOC_SET_BPF 注入，故 RwLock）。
    pub event_callback: RwLock<Option<Arc<dyn uprobe::CallBackFunc>>>,
}

/// 全局注册表：inode id → 文件偏移 → （消费者 id，回调）。
/// 注册表值类型：某（inode, offset）上的消费者列表。
type ConsumerList = Vec<(u64, Arc<UprobeConsumerReg>)>;
/// 注册表类型：inode id → （文件偏移 → 消费者列表）。
type RegistryMap = BTreeMap<usize, BTreeMap<usize, ConsumerList>>;

static UPROBE_REGISTRY: SpinLock<RegistryMap> = SpinLock::new(BTreeMap::new());

/// 迟到安装（fork/mmap）产生的句柄，按消费者 id 归档。
/// 消费者 close 时 drop → 逐 mm 注销。
static CONSUMER_LATE_HANDLES: SpinLock<BTreeMap<u64, Vec<UprobeHandle>>> =
    SpinLock::new(BTreeMap::new());

static NEXT_CONSUMER_ID: AtomicU64 = AtomicU64::new(1);

/// 分配新的消费者 id（每次 perf_event_open(uprobe) 一次）。
pub fn uprobe_new_consumer_id() -> u64 {
    NEXT_CONSUMER_ID.fetch_add(1, Ordering::Relaxed)
}

/// 注册一个消费者探测点（inode + offset）。
pub fn uprobe_registry_add(
    inode_id: usize,
    offset: usize,
    consumer_id: u64,
    reg: Arc<UprobeConsumerReg>,
) {
    let mut r = UPROBE_REGISTRY.lock_irqsave();
    r.entry(inode_id)
        .or_default()
        .entry(offset)
        .or_default()
        .push((consumer_id, reg));
}

/// 更新某消费者的 BPF 事件回调（PERF_EVENT_IOC_SET_BPF 时调用）。
/// 迟到安装的实例据此取得与直接安装一致的回调。
pub fn uprobe_registry_set_callback(consumer_id: u64, cb: Arc<dyn uprobe::CallBackFunc>) {
    let r = UPROBE_REGISTRY.lock_irqsave();
    for (_, offsets) in r.iter() {
        for (_, consumers) in offsets.iter() {
            for (id, reg) in consumers.iter() {
                if *id == consumer_id {
                    *reg.event_callback.write() = Some(cb.clone());
                }
            }
        }
    }
}
/// 消费者关闭：移除注册表项 + drop 迟到句柄（逐 mm 注销）。
pub fn uprobe_registry_remove_consumer(consumer_id: u64) {
    {
        let mut r = UPROBE_REGISTRY.lock_irqsave();
        for (_, offsets) in r.iter_mut() {
            for (_, consumers) in offsets.iter_mut() {
                consumers.retain(|(id, _)| *id != consumer_id);
            }
        }
    }
    // 取出迟到句柄（锁内只做 remove，drop 在锁外——Drop 路径会取 mm.write()）。
    let late = { CONSUMER_LATE_HANDLES.lock_irqsave().remove(&consumer_id) };
    drop(late);
}

/// 对新映射的文件 VMA 迟到应用注册表中的探针（评审 R9：dlopen / 后续 mmap）。
///
/// 在 mmap 提交且地址空间写锁释放后调用（本函数内部自取 `mm.write()`）。
/// `region_start/size` 为 VMA 的用户地址区间；`file_start_byte` 为 VMA 起始
/// 地址对应的文件偏移（= `backing_pgoff << PAGE_SHIFT`）。
pub fn uprobe_apply_to_new_vma(
    mm: &Arc<AddressSpace>,
    file: &Arc<crate::filesystem::vfs::file::File>,
    region_start: usize,
    region_size: usize,
    file_start_byte: usize,
) {
    let inode_id = match file.inode().metadata() {
        Ok(md) => md.inode_id.data(),
        Err(_) => return,
    };
    // 锁内快照：落在新 VMA 文件区间内的消费者列表
    let matches: Vec<(usize, ConsumerList)> = {
        let r = UPROBE_REGISTRY.lock_irqsave();
        let Some(offsets) = r.get(&inode_id) else {
            return;
        };
        offsets
            .iter()
            .filter(|(off, _)| **off >= file_start_byte && **off < file_start_byte + region_size)
            .map(|(off, c)| (*off, c.clone()))
            .collect()
    };
    if matches.is_empty() {
        return;
    }

    for (offset, consumers) in matches {
        let probe_vaddr = region_start + (offset - file_start_byte);
        for (consumer_id, reg) in consumers {
            // 该消费者在此 mm 的该地址是否已有实例（fork 继承可能已装）？
            let already = {
                let list = mm.uprobe_list.lock_irqsave();
                list.get(&probe_vaddr)
                    .is_some_and(|es| es.iter().any(|e| e.read().consumer_id == consumer_id))
            };
            if already {
                continue;
            }
            let basic_pre = reg.pre_handler;
            let basic_post = reg.post_handler;
            match uprobe_register(mm, probe_vaddr, basic_pre, basic_post, consumer_id) {
                Ok(handle) => {
                    // 注入 event_callback + 对齐使能状态
                    if let Some(inst) = handle.instance() {
                        let mut g = inst.write();
                        if let Some(cb) = reg.event_callback.read().clone() {
                            g.basic.update_event_callback(cb);
                        }
                    }
                    let mut late = CONSUMER_LATE_HANDLES.lock_irqsave();
                    late.entry(consumer_id).or_default().push(handle);
                }
                Err(e) => {
                    log::debug!(
                        "uprobe late-apply {:x}+{:#x} in new vma failed: {:?}",
                        inode_id,
                        offset,
                        e
                    );
                }
            }
        }
    }
}

/// fork 时把父 mm 的探针继承到子 mm（评审 R9）。
///
/// 在 clone 完成、父 mm 全部锁释放后调用；子 mm 尚无运行线程。
/// 子页经 fork 已含父页的 0xcc（共享只读映射），此处将其私有化并重建
/// per-mm 实例（slot/表项），沿用父实例的 consumer_id（消费者 close 一并注销）。
pub fn fork_inherit_uprobes(parent_mm: &Arc<AddressSpace>, child_mm: &Arc<AddressSpace>) {
    // 1. 快照父表
    let snapshot: Vec<(usize, Vec<Arc<RwLock<UprobeInstance>>>)> = {
        let list = parent_mm.uprobe_list.lock_irqsave();
        list.iter().map(|(k, v)| (*k, v.clone())).collect()
    };
    if snapshot.is_empty() {
        return;
    }

    let mut late_by_consumer: BTreeMap<u64, Vec<UprobeHandle>> = BTreeMap::new();
    let mut child_inner = child_mm.write();

    for (probe_vaddr, entries) in snapshot {
        for entry in entries {
            let inst = entry.read();
            let (pre, post) = inst.basic.handlers();
            let cb = inst.basic.event_callback_arc();
            let enabled = inst.basic.is_enabled();
            let Some(pp) = inst.basic.probe_point() else {
                continue;
            };
            match uprobe_inherit_instance(
                child_mm,
                &mut child_inner,
                probe_vaddr,
                pp.old_instruction,
                pp.insn_len,
                inst.insn_analysis,
                pre,
                post,
                cb,
                enabled,
                inst.consumer_id,
            ) {
                Ok(handle) => late_by_consumer
                    .entry(inst.consumer_id)
                    .or_default()
                    .push(handle),
                Err(e) => log::warn!(
                    "uprobe fork-inherit {:#x} (consumer {}) failed: {:?}",
                    probe_vaddr,
                    inst.consumer_id,
                    e
                ),
            }
        }
    }
    drop(child_inner);

    // 2. 句柄归档（消费者 close 时逐 mm 注销）
    if !late_by_consumer.is_empty() {
        let mut late = CONSUMER_LATE_HANDLES.lock_irqsave();
        for (cid, handles) in late_by_consumer {
            late.entry(cid).or_default().extend(handles);
        }
    }
}

/// 在子 mm 重建一个继承的 uprobe 实例（不重读指令——子页含 0xcc）。
#[allow(clippy::too_many_arguments)]
fn uprobe_inherit_instance(
    mm: &Arc<AddressSpace>,
    inner: &mut RwSemWriteGuard<'_, InnerAddressSpace>,
    probe_vaddr: usize,
    old_instruction: [u8; UPROBE_INSN_COPY_SIZE],
    insn_len: usize,
    insn_analysis: InsnAnalysis,
    pre_handler: fn(&dyn ProbeArgs),
    post_handler: fn(&dyn ProbeArgs),
    event_callback: Option<Arc<dyn uprobe::CallBackFunc>>,
    enabled: bool,
    consumer_id: u64,
) -> Result<UprobeHandle, SystemError> {
    // ── slot 分配 + 预填（镜像 uprobe_register Step 2/P2）──
    let xol_slot_offset = ensure_xol_and_alloc_slot(mm, inner)?;
    {
        let (slot_vaddr, page_paddr) = {
            let guard = mm.xol_area.lock_irqsave();
            let area = guard.as_ref().ok_or(SystemError::EFAULT)?;
            (area.slot_vaddr(xol_slot_offset), area.page_paddr())
        };
        let mut slot_buf = [0u8; UPROBE_INSN_COPY_SIZE];
        if let Err(e) = build_xol_slot(
            &insn_analysis,
            probe_vaddr,
            slot_vaddr.data(),
            &old_instruction,
            &mut slot_buf,
        ) {
            log::warn!("uprobe fork-inherit: build_xol_slot failed: {:?}", e);
            free_xol_slot(mm, xol_slot_offset);
            return Err(SystemError::EINVAL);
        }
        let kva = unsafe { MMArch::phys_2_virt(page_paddr) }.ok_or_else(|| {
            free_xol_slot(mm, xol_slot_offset);
            SystemError::EFAULT
        })?;
        unsafe {
            let dst = (kva.data() + xol_slot_offset) as *mut u8;
            core::ptr::copy_nonoverlapping(slot_buf.as_ptr(), dst, UPROBE_INSN_COPY_SIZE);
        }
    }

    // ── 实体 ──
    let mut point = UprobePoint::new(probe_vaddr);
    point.old_instruction = old_instruction;
    point.insn_len = insn_len;
    point.xol_slot_offset = xol_slot_offset;
    let mut builder = UprobeBuilder::new(probe_vaddr, pre_handler, post_handler, enabled)
        .with_probe_point(Arc::new(point));
    if let Some(cb) = event_callback {
        builder = builder.with_event_callback(cb);
    }
    let basic = builder.build();
    let entry = Arc::new(RwLock::new(UprobeInstance {
        basic,
        insn_analysis,
        consumer_id,
    }));

    // ── 表项（0xcc 已在页上——fork 继承，发布顺序无窗口）──
    {
        let mut list = mm.uprobe_list.lock_irqsave();
        list.entry(probe_vaddr).or_default().push(entry.clone());
    }

    // ── 页私有化：子页当前与父共享（含 0xcc）；COW 为子私有副本，
    //    使后续子 mm 的注销恢复字节不写入共享页 ──
    privatize_inherited_page(mm, inner, probe_vaddr)?;

    Ok(UprobeHandle {
        mm: Arc::downgrade(mm),
        probe_vaddr,
        entry: Some(entry),
    })
}

/// 把子 mm 中一个继承断点页私有化（copy → set_entry → rmap）。
///
/// 若该页已私有化（同页多探针），仅 refcount++。0xcc 字节随拷贝带入，
/// 无需再 patch。
fn privatize_inherited_page(
    mm: &Arc<AddressSpace>,
    inner: &mut RwSemWriteGuard<'_, InnerAddressSpace>,
    probe_vaddr: usize,
) -> Result<(), SystemError> {
    let page_base_addr = probe_vaddr & !(MMArch::PAGE_SIZE - 1);
    let address = VirtAddr::new(page_base_addr);
    let end = VirtAddr::new(page_base_addr + MMArch::PAGE_SIZE);

    {
        let mut pb = mm.uprobe_page_state.lock_irqsave();
        if let Some(state) = pb.get_mut(&page_base_addr) {
            state.refcount += 1;
            return Ok(());
        }
    }

    let _pt_edit = mm.page_table_edit();
    let mapper = &mut inner.user_mapper.utable;
    let (old_paddr, entry_flags) = mapper.translate(address).ok_or(SystemError::EFAULT)?;
    let old_page = {
        let mut pm = page_manager_lock();
        pm.get(&old_paddr).ok_or(SystemError::EFAULT)?
    };
    let new_page = {
        let mut pm = page_manager_lock();
        pm.copy_page_as_normal(&old_paddr, mapper.allocator_mut())
            .map_err(|_| SystemError::ENOMEM)?
    };
    let table = mapper.get_table(address, 0).ok_or(SystemError::EFAULT)?;
    let i = table.index_of(address).ok_or(SystemError::EFAULT)?;
    unsafe {
        table.set_entry(i, PageEntry::new(new_page.phys_address(), entry_flags));
    }
    mm.flush_tlb_range(address, end, MMArch::PAGE_SHIFT as u8, false);

    // rmap 账簿
    if let Some(vma) = inner.mappings.contains(address) {
        let vm_locked = vma.lock().vm_flags().contains(VmFlags::VM_LOCKED);
        new_page.write().insert_vma(vma.clone(), vm_locked);
        {
            let mut old_guard = old_page.write();
            old_guard.remove_vma(vma.as_ref());
        }
        InnerAddressSpace::remove_page_unevictable_if_unneeded(&old_page);
    }

    let mut pb = mm.uprobe_page_state.lock_irqsave();
    pb.insert(page_base_addr, UprobePageState { refcount: 1 });
    Ok(())
}
