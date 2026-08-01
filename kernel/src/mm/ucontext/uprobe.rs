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

use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
};

use system_error::SystemError;

use crate::{
    arch::{mm::PageMapper, MMArch},
    libs::rwlock::RwLock,
    mm::{
        page::{page_manager_lock, EntryFlags as PageEntryFlags, Page, PageEntry},
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
const XOL_BITMAP_WORDS: usize = (XOL_SLOTS_PER_PAGE + 63) / 64;

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
}

impl core::fmt::Debug for UprobeInstance {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UprobeInstance")
            .field("probe_vaddr", &self.basic.probe_vaddr())
            .field("insn_analysis", &self.insn_analysis)
            .finish()
    }
}

// ──────────────────── per-page 断点页追踪 ────────────────────

/// 某个页上已安装断点的状态（用于注销时恢复原页）。
///
/// 以页基地址（`probe_vaddr & !(PAGE_SIZE-1)`）为键。多个 uprobe 命中同一页时共享
/// 一个 COW 副本，`refcount` 记录活跃断点数；降到 0 时恢复原页。
pub(crate) struct UprobePageState {
    /// 原始物理页（COW 之前的共享/文件页）。
    original_page: Arc<Page>,
    /// COW 副本（已 patch 0xcc）。
    cow_page: Arc<Page>,
    /// 原始 PTE flags（恢复时用）。
    original_flags: PageEntryFlags<MMArch>,
    /// 活跃断点数。
    refcount: usize,
}

impl UprobePageState {
    /// 原始物理页地址（供 try_clone 替换子进程的断点页）。
    pub(crate) fn original_paddr(&self) -> PhysAddr {
        self.original_page.phys_address()
    }
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
/// ## pid 语义
/// 本函数操作**单个** mm。`pid == -1`（经 inode rmap 全量 mm）留待 batch4 协调；
/// 届时 batch4 遍历目标 inode 的所有 VMA，对每个 mm 调用本函数。
pub fn uprobe_register(
    mm: &Arc<AddressSpace>,
    probe_vaddr: usize,
    pre_handler: fn(&dyn ProbeArgs),
    post_handler: fn(&dyn ProbeArgs),
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
        let mut old_instruction = [0u8; UPROBE_INSN_COPY_SIZE];
        let avail = MMArch::PAGE_SIZE - page_offset;
        let copy_len = avail.min(analysis.insn_len);
        old_instruction[..copy_len].copy_from_slice(&insn_bytes[..copy_len]);
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
    let (paddr, _flags) = mapper
        .translate(VirtAddr::new(probe_vaddr))
        .ok_or(SystemError::EFAULT)?;

    let kva = unsafe { MMArch::phys_2_virt(paddr) }.ok_or(SystemError::EFAULT)?;
    let avail = MMArch::PAGE_SIZE - page_offset;
    let read_len = avail.min(UPROBE_INSN_COPY_SIZE);

    let mut bytes = [0u8; UPROBE_INSN_COPY_SIZE];
    unsafe {
        core::ptr::copy_nonoverlapping(
            (kva.data() + page_offset) as *const u8,
            bytes.as_mut_ptr(),
            read_len,
        );
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
    let existing_cow = {
        let pb = mm.uprobe_page_state.lock_irqsave();
        pb.get(&page_base_addr).map(|s| s.cow_page.clone())
    };

    if let Some(cow_page) = existing_cow {
        // 页已有 COW 副本：只需在副本中 patch 额外 0xcc 字节
        patch_byte_in_phys(&cow_page, page_offset, 0xcc)?;
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

    // 记录页状态（供注销恢复）
    let mut pb = mm.uprobe_page_state.lock_irqsave();
    pb.insert(
        page_base_addr,
        UprobePageState {
            original_page: old_page,
            cow_page: new_page,
            original_flags: entry_flags,
            refcount: 1,
        },
    );

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

/// 注销内部实现（反序：移除表项 → 恢复页 → 回收 slot）。
fn uprobe_unregister_internal(
    mm: &Arc<AddressSpace>,
    probe_vaddr: usize,
    entry: &Arc<RwLock<UprobeInstance>>,
) {
    // ── 1. 从 uprobe_list 移除（断点不再命中）──
    let removed = {
        let mut list = mm.uprobe_list.lock_irqsave();
        let mut removed = false;
        if let Some(entries) = list.get_mut(&probe_vaddr) {
            let before = entries.len();
            entries.retain(|x| !Arc::ptr_eq(x, entry));
            removed = entries.len() < before;
            if entries.is_empty() {
                list.remove(&probe_vaddr);
            }
        }
        removed
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

    // ── 2. 恢复断点页 ──
    let page_base_addr = probe_vaddr & !(MMArch::PAGE_SIZE - 1);
    let page_offset = probe_vaddr & (MMArch::PAGE_SIZE - 1);

    {
        let mut inner = mm.write();
        restore_breakpoint_page(mm, &mut inner, page_base_addr, page_offset, orig_first_byte);
    }

    // ── 3. 回收 XOL slot ──
    if let Some(offset) = xol_slot_offset {
        free_xol_slot(mm, offset);
    }
}

/// 恢复断点页（注销时调用）。
///
/// 1. 在 COW 副本中恢复该偏移处的原指令首字节（清除 0xcc）。
/// 2. refcount--；若降到 0，恢复原始物理页（set_entry + rmap + flush_tlb）。
fn restore_breakpoint_page(
    mm: &Arc<AddressSpace>,
    inner: &mut RwSemWriteGuard<'_, InnerAddressSpace>,
    page_base_addr: usize,
    page_offset: usize,
    orig_first_byte: u8,
) {
    // 决定是否需要恢复整页
    let restore_info = {
        let mut pb = mm.uprobe_page_state.lock_irqsave();
        let Some(state) = pb.get_mut(&page_base_addr) else {
            return; // 页无断点状态——可能已被 munmap 清理
        };

        // 先在 COW 副本中恢复该字节（清除 0xcc，不影响同页其他 uprobe）
        if let Some(kva) = unsafe { MMArch::phys_2_virt(state.cow_page.phys_address()) } {
            unsafe {
                core::ptr::write_volatile((kva.data() + page_offset) as *mut u8, orig_first_byte);
            }
        }

        state.refcount = state.refcount.saturating_sub(1);
        if state.refcount > 0 {
            return; // 仍有其他 uprobe 在此页
        }

        // refcount == 0：取出状态，恢复整页
        pb.remove(&page_base_addr).unwrap()
    };

    // 恢复原始物理页
    let address = VirtAddr::new(page_base_addr);
    let end = VirtAddr::new(page_base_addr + MMArch::PAGE_SIZE);

    // 检查 PTE 是否仍存在（可能已被 munmap）
    let _pt_edit = mm.page_table_edit();
    let mapper = &mut inner.user_mapper.utable;

    let table = match mapper.get_table(address, 0) {
        Some(t) => t,
        None => return, // 页表已被拆除（munmap），无需恢复 PTE
    };
    let i = match table.index_of(address) {
        Some(i) => i,
        None => return,
    };

    // 确认当前 PTE 指向 COW 副本（否则可能已被其他操作替换）
    let current_entry = unsafe { table.entry(i) };
    if let Some(entry) = current_entry {
        if entry.address() != Ok(restore_info.cow_page.phys_address()) {
            // PTE 已不指向 COW 副本——可能是 write fault 已做了二次 COW。
            // 不恢复，仅清理 page_state（已 remove）。COW 副本 Arc drop 后回收。
            return;
        }
    }

    // 单次 set_entry 恢复原页
    unsafe {
        table.set_entry(
            i,
            PageEntry::new(
                restore_info.original_page.phys_address(),
                restore_info.original_flags,
            ),
        );
    }

    mm.flush_tlb_range(address, end, MMArch::PAGE_SHIFT as u8, false);

    // rmap：尝试 attach 原页、detach COW 副本（需要 VMA）
    let vma = inner.mappings.contains(address);
    if let Some(vma) = vma {
        let vm_locked = vma.lock().vm_flags().contains(VmFlags::VM_LOCKED);
        restore_info
            .original_page
            .write()
            .insert_vma(vma.clone(), vm_locked);
        {
            let mut cow_guard = restore_info.cow_page.write();
            cow_guard.remove_vma(vma.as_ref());
        }
        InnerAddressSpace::remove_page_unevictable_if_unneeded(&restore_info.cow_page);
    }
    // 若 VMA 不存在（已 munmap），原页 rmap 不变，COW 副本 Arc drop 后回收。
}

// ──────────────────────── 辅助：空操作 handler ────────────────────────

/// 空操作 handler（供 batch4 在仅需 event_callback 时占位）。
pub fn noop_handler(_args: &dyn ProbeArgs) {}
