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
use crate::libs::{spinlock::SpinLock, wait_queue::WaitQueue};
use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};

use system_error::SystemError;

use crate::{
    arch::{mm::PageMapper, MMArch},
    filesystem::{page_cache::PageCache, vfs::IndexNode},
    libs::{mutex::Mutex, rwlock::RwLock},
    mm::{
        page::{page_manager_lock, Page, PageEntry},
        syscall::{MapFlags, ProtFlags},
        MemoryManagementArch, PhysAddr, VirtAddr, VirtRegion, VmFlags,
    },
    process::ProcessControlBlock,
};

use super::RwSemWriteGuard;
use super::{AddressSpace, InnerAddressSpace, LockedVMA};

use uprobe::{
    analyze_insn, build_xol_slot, InsnAnalysis, ProbeArgs, UprobePoint, UPROBE_INSN_COPY_SIZE,
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
    /// 保证 XOL 物理页覆盖整个租约生命周期；不能只保存裸物理地址。
    _page: Arc<Page>,
    /// 区域代次，用于阻止旧租约释放新区域的同号 slot。
    generation: u64,
    /// slot 分配位图（bit=1 表示已占用）。
    slot_bitmap: SpinLock<[u64; XOL_BITMAP_WORDS]>,
}

impl XolArea {
    fn alloc_slot(self: &Arc<Self>) -> Option<XolSlotLease> {
        let mut bitmap = self.slot_bitmap.lock_irqsave();
        for (word_idx, word) in bitmap.iter_mut().enumerate() {
            if *word != u64::MAX {
                let bit = (!*word).trailing_zeros() as usize;
                let slot = word_idx * 64 + bit;
                if slot >= XOL_SLOTS_PER_PAGE {
                    break;
                }
                *word |= 1u64 << bit;
                return Some(XolSlotLease {
                    area: self.clone(),
                    offset: slot * XOL_SLOT_SIZE,
                    generation: self.generation,
                });
            }
        }
        None
    }

    fn free_slot(&self, offset: usize, generation: u64) {
        if generation != self.generation {
            return;
        }
        let slot = offset / XOL_SLOT_SIZE;
        if slot < XOL_SLOTS_PER_PAGE {
            self.slot_bitmap.lock_irqsave()[slot / 64] &= !(1u64 << (slot % 64));
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

/// 一个 XOL slot 的唯一所有权租约。命中路径应把 `Arc<XolSlotLease>` 放入
/// `ActiveXol`，从而让注销只撤销后续命中，不能复用仍在执行的 slot。
pub struct XolSlotLease {
    area: Arc<XolArea>,
    offset: usize,
    generation: u64,
}

impl XolSlotLease {
    pub fn offset(&self) -> usize {
        self.offset
    }

    pub fn slot_vaddr(&self) -> VirtAddr {
        self.area.slot_vaddr(self.offset)
    }

    pub fn page_paddr(&self) -> PhysAddr {
        self.area.page_paddr()
    }

    pub fn area(&self) -> &Arc<XolArea> {
        &self.area
    }
}

impl Drop for XolSlotLease {
    fn drop(&mut self) {
        self.area.free_slot(self.offset, self.generation);
    }
}

impl core::fmt::Debug for XolSlotLease {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("XolSlotLease")
            .field("offset", &self.offset)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

static NEXT_XOL_GENERATION: AtomicU64 = AtomicU64::new(1);

/// Linux `struct uprobe` 的定义域：文件对象与文件偏移唯一确定探针，指令只分析一次。
pub struct UprobeDefinition {
    inode: Arc<dyn IndexNode>,
    page_cache: Arc<PageCache>,
    inode_id: usize,
    inode_key: usize,
    offset: usize,
    old_instruction: [u8; UPROBE_INSN_COPY_SIZE],
    analysis: InsnAnalysis,
}

impl UprobeDefinition {
    pub fn new(inode: Arc<dyn IndexNode>, offset: usize) -> Result<Arc<Self>, SystemError> {
        let metadata = inode.metadata()?;
        let file_size = usize::try_from(metadata.size).map_err(|_| SystemError::EINVAL)?;
        if offset >= file_size {
            return Err(SystemError::EINVAL);
        }
        let inode_id = metadata.inode_id.data();
        let page_cache = inode.page_cache().ok_or(SystemError::EINVAL)?;
        // Mount wrappers and hardlink dentries may expose different IndexNode
        // Arcs for the same underlying inode. The shared page cache is the
        // canonical file-mapping identity used by every mmap/rmap path.
        let inode_key = Arc::as_ptr(&page_cache) as usize;
        {
            let definitions = UPROBE_DEFINITIONS.lock_irqsave();
            if let Some(existing) = definitions
                .get(&(inode_key, offset))
                .and_then(Weak::upgrade)
            {
                return Ok(existing);
            }
        }

        // Linux copies the definition instruction from the file mapping, not
        // from a particular process's possibly private/COW mapping. This also
        // allows a valid instruction to straddle a page or adjacent VMAs.
        let available = (file_size - offset).min(UPROBE_INSN_COPY_SIZE);
        let mut bytes = [0u8; UPROBE_INSN_COPY_SIZE];
        let read = page_cache.read(offset, &mut bytes[..available])?;
        if read == 0 {
            return Err(SystemError::EIO);
        }
        let analysis = analyze_insn(&bytes).map_err(|_| SystemError::EINVAL)?;
        if analysis.insn_len > read {
            return Err(SystemError::EINVAL);
        }
        let mut old_instruction = [0; UPROBE_INSN_COPY_SIZE];
        old_instruction[..analysis.insn_len].copy_from_slice(&bytes[..analysis.insn_len]);

        let definition = Arc::new(Self {
            inode,
            page_cache,
            inode_id,
            inode_key,
            offset,
            old_instruction,
            analysis,
        });
        let mut definitions = UPROBE_DEFINITIONS.lock_irqsave();
        if let Some(existing) = definitions
            .get(&(inode_key, offset))
            .and_then(Weak::upgrade)
        {
            return Ok(existing);
        }
        definitions.insert((inode_key, offset), Arc::downgrade(&definition));
        Ok(definition)
    }

    pub fn inode(&self) -> &Arc<dyn IndexNode> {
        &self.inode
    }

    fn matches_inode(&self, inode: &Arc<dyn IndexNode>) -> bool {
        inode
            .page_cache()
            .is_some_and(|page_cache| Arc::ptr_eq(&page_cache, &self.page_cache))
    }

    pub fn inode_id(&self) -> usize {
        self.inode_id
    }

    pub fn offset(&self) -> usize {
        self.offset
    }

    fn instruction(&self) -> ([u8; UPROBE_INSN_COPY_SIZE], InsnAnalysis) {
        (self.old_instruction, self.analysis)
    }
}

impl Drop for UprobeDefinition {
    fn drop(&mut self) {
        let key = (self.inode_key, self.offset);
        let self_ptr = core::ptr::from_ref(self);
        let mut definitions = UPROBE_DEFINITIONS.lock_irqsave();
        if definitions
            .get(&key)
            .is_some_and(|weak| core::ptr::eq(weak.as_ptr(), self_ptr))
        {
            definitions.remove(&key);
        }
    }
}

#[derive(Clone)]
pub struct UprobeTaskScope(Arc<UprobeTaskScopeToken>);

/// The global weak reference keeps the PCB allocation from being reused while
/// a scope exists. The pointer cookie can therefore be compared on the hit
/// path without taking the global scope lock.
struct UprobeTaskScopeToken {
    id: u64,
    target_ptr: usize,
}

impl Drop for UprobeTaskScopeToken {
    fn drop(&mut self) {
        UPROBE_TASK_SCOPES.lock_irqsave().remove(&self.id);
    }
}

impl UprobeTaskScope {
    pub fn new(target: &Arc<ProcessControlBlock>) -> Self {
        let id = NEXT_TASK_SCOPE_ID.fetch_add(1, Ordering::Relaxed);
        UPROBE_TASK_SCOPES
            .lock_irqsave()
            .insert(id, Arc::downgrade(target));
        Self(Arc::new(UprobeTaskScopeToken {
            id,
            target_ptr: Arc::as_ptr(target) as usize,
        }))
    }

    fn permits_task(&self, current: &Arc<ProcessControlBlock>) -> bool {
        Arc::as_ptr(current) as usize == self.0.target_ptr
    }

    fn target_mm(&self) -> Option<Arc<AddressSpace>> {
        let target = { UPROBE_TASK_SCOPES.lock_irqsave().get(&self.0.id).cloned() };
        target
            .and_then(|target| target.upgrade())
            .and_then(|task| task.basic().user_vm())
    }
}

pub enum UprobeConsumerScope {
    Task(UprobeTaskScope),
    SystemWideAuthorized,
}

impl UprobeConsumerScope {
    fn permits(&self, mm: &Arc<AddressSpace>) -> bool {
        match self {
            Self::Task(target) => target
                .target_mm()
                .is_some_and(|target_mm| Arc::ptr_eq(&target_mm, mm)),
            Self::SystemWideAuthorized => true,
        }
    }

    fn task_scope(&self) -> Option<UprobeTaskScope> {
        match self {
            Self::Task(scope) => Some(scope.clone()),
            Self::SystemWideAuthorized => None,
        }
    }
}

pub struct UprobeConsumerRuntime {
    pub pre_handler: fn(&dyn ProbeArgs),
    pub post_handler: fn(&dyn ProbeArgs),
    pub event_callback: Option<Arc<dyn uprobe::CallBackFunc>>,
    pub enabled: bool,
}

#[derive(Clone)]
pub struct UprobeConsumerRuntimeSnapshot {
    pub pre_handler: fn(&dyn ProbeArgs),
    pub post_handler: fn(&dyn ProbeArgs),
    pub event_callback: Option<Arc<dyn uprobe::CallBackFunc>>,
    task_scope: Option<UprobeTaskScope>,
}

impl UprobeConsumerRuntimeSnapshot {
    pub fn permits_task(&self, current: &Arc<ProcessControlBlock>) -> bool {
        self.task_scope
            .as_ref()
            .is_none_or(|scope| scope.permits_task(current))
    }
}

struct InstalledSiteRef {
    mm: Weak<AddressSpace>,
    vaddr: usize,
    site: Weak<UprobeSite>,
}

pub struct UprobeConsumer {
    id: u64,
    definition: Arc<UprobeDefinition>,
    scope: UprobeConsumerScope,
    runtime: RwLock<UprobeConsumerRuntime>,
    enabled: AtomicBool,
    lifecycle: Mutex<()>,
    closing: AtomicBool,
    inflight: AtomicUsize,
    inflight_wait: WaitQueue,
    sites: SpinLock<Vec<InstalledSiteRef>>,
}

struct ConsumerInstallGuard<'a>(&'a UprobeConsumer);

impl Drop for ConsumerInstallGuard<'_> {
    fn drop(&mut self) {
        if self.0.inflight.fetch_sub(1, Ordering::Release) == 1 {
            self.0.inflight_wait.wakeup_all(None);
        }
    }
}

impl UprobeConsumer {
    pub fn new(
        id: u64,
        definition: Arc<UprobeDefinition>,
        scope: UprobeConsumerScope,
        runtime: UprobeConsumerRuntime,
    ) -> Arc<Self> {
        let enabled = runtime.enabled;
        Arc::new(Self {
            id,
            definition,
            scope,
            runtime: RwLock::new(runtime),
            enabled: AtomicBool::new(enabled),
            lifecycle: Mutex::new(()),
            closing: AtomicBool::new(false),
            inflight: AtomicUsize::new(0),
            inflight_wait: WaitQueue::default(),
            sites: SpinLock::new(Vec::new()),
        })
    }

    fn begin_install(&self, mm: &Arc<AddressSpace>) -> Option<ConsumerInstallGuard<'_>> {
        if !self.scope.permits(mm)
            || self.closing.load(Ordering::Acquire)
            || !self.enabled.load(Ordering::Acquire)
        {
            return None;
        }
        self.inflight.fetch_add(1, Ordering::AcqRel);
        if self.closing.load(Ordering::Acquire) || !self.enabled.load(Ordering::Acquire) {
            if self.inflight.fetch_sub(1, Ordering::Release) == 1 {
                self.inflight_wait.wakeup_all(None);
            }
            return None;
        }
        Some(ConsumerInstallGuard(self))
    }

    fn remember_site(&self, mm: &Arc<AddressSpace>, vaddr: usize, site: &Arc<UprobeSite>) {
        let mut sites = self.sites.lock_irqsave();
        let mut already_present = false;
        sites.retain(|installed| {
            let (Some(installed_mm), Some(installed_site)) =
                (installed.mm.upgrade(), installed.site.upgrade())
            else {
                return false;
            };
            if installed_site.state() == UprobeSiteState::Dead {
                return false;
            }
            if installed.vaddr == vaddr
                && Arc::ptr_eq(&installed_mm, mm)
                && Arc::ptr_eq(&installed_site, site)
            {
                already_present = true;
            }
            true
        });
        if already_present {
            return;
        }
        sites.push(InstalledSiteRef {
            mm: Arc::downgrade(mm),
            vaddr,
            site: Arc::downgrade(site),
        });
    }

    /// Runtime state belongs to the perf consumer, not to one particular VMA
    /// instance.  Current and future sites therefore observe the same
    /// enable/callback state.
    pub fn runtime_snapshot(&self) -> Option<UprobeConsumerRuntimeSnapshot> {
        let runtime = self.runtime.read();
        if self.closing.load(Ordering::Acquire) || !self.enabled.load(Ordering::Acquire) {
            return None;
        }
        Some(UprobeConsumerRuntimeSnapshot {
            pre_handler: runtime.pre_handler,
            post_handler: runtime.post_handler,
            event_callback: runtime.event_callback.clone(),
            task_scope: self.scope.task_scope(),
        })
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UprobeSiteState {
    Prepared = 0,
    Armed = 1,
    Disarming = 2,
    Dead = 3,
}

pub struct UprobeSite {
    pub definition: Arc<UprobeDefinition>,
    pub probe_vaddr: usize,
    mapping: Weak<LockedVMA>,
    mapping_state_seq: u64,
    pub xol_lease: Arc<XolSlotLease>,
    /// Immutable hit-path snapshot. Writers rebuild it in process context;
    /// #BP only clones the Arc and never allocates.
    pub participants: RwLock<Arc<Vec<UprobeConsumerRuntimeSnapshot>>>,
    state: AtomicU8,
}

impl UprobeSite {
    pub fn state(&self) -> UprobeSiteState {
        match self.state.load(Ordering::Acquire) {
            0 => UprobeSiteState::Prepared,
            1 => UprobeSiteState::Armed,
            2 => UprobeSiteState::Disarming,
            _ => UprobeSiteState::Dead,
        }
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
    pub point: Arc<UprobePoint>,
    /// x86 指令静态分析（命中时供 `build_xol_slot` 用）。
    pub insn_analysis: InsnAnalysis,
    /// 拥有此实例的消费者（perf event fd）id（评审 R9）。
    /// fork 继承的子实例沿用父实例的 id，使消费者 close 时一并注销。
    pub consumer_id: u64,
    /// 保持 site 的 XOL slot，后续异常路径迁移后还会把它克隆进 ActiveXol。
    pub xol_lease: Arc<XolSlotLease>,
    pub site: Arc<UprobeSite>,
    pub consumer: Arc<UprobeConsumer>,
}

impl core::fmt::Debug for UprobeInstance {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UprobeInstance")
            .field("probe_vaddr", &self.point.probe_vaddr)
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
/// Drop 时自动撤销尚未转交给 mm 命中表的安装。
pub struct UprobeHandle {
    mm: Weak<AddressSpace>,
    probe_vaddr: usize,
    entry: Option<Arc<RwLock<UprobeInstance>>>,
}

impl UprobeHandle {
    /// 把所有权转交给 consumer 的弱 site 索引；用于 mmap/fork 的持久安装。
    fn persist(mut self) {
        self.entry.take();
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

struct ExpectedProbeVma {
    vma: Weak<LockedVMA>,
    state_seq: u64,
    region: VirtRegion,
}

struct ExpectedProbeMapping {
    parts: Vec<ExpectedProbeVma>,
}

fn valid_probe_vma_flags(flags: VmFlags) -> bool {
    let invalid = VmFlags::VM_HUGETLB | VmFlags::VM_MAYSHARE | VmFlags::VM_WRITE;
    flags.contains(VmFlags::VM_EXEC)
        && flags.contains(VmFlags::VM_MAYEXEC)
        && !flags.intersects(invalid)
}

fn probe_mapping_parts(
    inner: &InnerAddressSpace,
    definition: &UprobeDefinition,
    probe_vaddr: usize,
) -> Option<Vec<(Arc<LockedVMA>, u64, VirtRegion)>> {
    let instruction_end = probe_vaddr.checked_add(definition.analysis.insn_len)?;
    let mut cursor = probe_vaddr;
    let mut parts = Vec::new();

    while cursor < instruction_end {
        let vma = inner.mappings.contains(VirtAddr::new(cursor))?;
        let guard = vma.lock();
        if !valid_probe_vma_flags(*guard.vm_flags()) {
            return None;
        }
        let file = guard.vm_file()?;
        if !definition.matches_inode(&file.inode()) {
            return None;
        }
        let pgoff = guard.backing_page_offset()?;
        let mapped_offset = pgoff
            .checked_mul(MMArch::PAGE_SIZE)?
            .checked_add(cursor.checked_sub(guard.region().start().data())?)?;
        let expected_offset = definition
            .offset()
            .checked_add(cursor.checked_sub(probe_vaddr)?)?;
        if mapped_offset != expected_offset {
            return None;
        }
        let part_end = guard.region().end().data().min(instruction_end);
        if part_end <= cursor {
            return None;
        }
        let state_seq = vma.state_seq();
        drop(guard);
        parts.push((
            vma,
            state_seq,
            VirtRegion::new(VirtAddr::new(cursor), part_end - cursor),
        ));
        cursor = part_end;
    }
    Some(parts)
}

fn capture_probe_mapping(
    inner: &InnerAddressSpace,
    definition: &UprobeDefinition,
    probe_vaddr: usize,
) -> Option<ExpectedProbeMapping> {
    let parts = probe_mapping_parts(inner, definition, probe_vaddr)?
        .into_iter()
        .map(|(vma, state_seq, region)| ExpectedProbeVma {
            vma: Arc::downgrade(&vma),
            state_seq,
            region,
        })
        .collect();
    Some(ExpectedProbeMapping { parts })
}

fn revalidate_probe_mapping(
    inner: &InnerAddressSpace,
    definition: &UprobeDefinition,
    probe_vaddr: usize,
    expected: &ExpectedProbeMapping,
) -> Option<Arc<LockedVMA>> {
    let current = probe_mapping_parts(inner, definition, probe_vaddr)?;
    if current.len() != expected.parts.len() {
        return None;
    }
    for ((vma, state_seq, region), expected) in current.iter().zip(&expected.parts) {
        let expected_vma = expected.vma.upgrade()?;
        if !Arc::ptr_eq(vma, &expected_vma)
            || *state_seq != expected.state_seq
            || *region != expected.region
        {
            return None;
        }
    }
    current.first().map(|(vma, _, _)| vma.clone())
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
fn uprobe_register(
    mm: &Arc<AddressSpace>,
    probe_vaddr: usize,
    _pre_handler: fn(&dyn ProbeArgs),
    _post_handler: fn(&dyn ProbeArgs),
    consumer: &Arc<UprobeConsumer>,
    expected_mapping: &ExpectedProbeMapping,
) -> Result<Option<UprobeHandle>, SystemError> {
    let _install = consumer.begin_install(mm).ok_or(SystemError::ENOENT)?;
    let consumer_id = consumer.id;
    // ── 持有 inner.write() 整个注册过程 ──
    let mut inner = mm.write();

    // ── Step 1: 定位 VMA + 读原指令 + 分析 ──
    let page_base_addr = probe_vaddr & !(MMArch::PAGE_SIZE - 1);
    let page_offset = probe_vaddr & (MMArch::PAGE_SIZE - 1);

    // Fault-in runs without mm.write(), so MAP_FIXED/mprotect can replace any
    // part of a cross-page instruction. Revalidate the complete executable,
    // canonical-file, contiguous-offset mapping chain in this stable view.
    let Some(vma) =
        revalidate_probe_mapping(&inner, &consumer.definition, probe_vaddr, expected_mapping)
    else {
        return Ok(None);
    };

    // ── P1：重复注册同一 probe_vaddr 时复用已有指令信息（避免读到 0xcc）──
    // 第二个 consumer 注册同一地址时 PTE 已指向含 0xcc 的 COW 副本，
    // read_user_insn_bytes 会把 0xcc 当原指令。故先查 uprobe_list：若有同址条目，
    // 复用其 old_instruction + insn_analysis（二者对所有同址实例一致），跳过读取。
    let reused = {
        let list = mm.uprobe_list.lock_irqsave();
        match list.get(&probe_vaddr) {
            Some(entries) => {
                // All registration paths reach this point under mm.write().
                // Keep the idempotence check in that serialization domain:
                // an enable scan and a post-mmap apply cannot both insert the
                // same consumer at this address.
                if entries
                    .iter()
                    .any(|entry| entry.read().consumer_id == consumer_id)
                {
                    return Ok(None);
                }
                entries.first().map(|entry| {
                    let inst = entry.read();
                    (
                        inst.point.old_instruction,
                        inst.insn_analysis,
                        inst.site.clone(),
                    )
                })
            }
            None => None,
        }
    };

    let (old_instruction, analysis, existing_site) = if let Some((oi, an, site)) = reused {
        if !Arc::ptr_eq(&site.definition, &consumer.definition) {
            return Err(SystemError::EINVAL);
        }
        (oi, an, Some(site))
    } else {
        // XOL must execute the bytes that native instruction fetch would have
        // observed. Linux verifies the software-breakpoint-sized opcode; we
        // additionally compare the complete decoded instruction so a private
        // alias or unrelated adjacent VMA cannot change execution semantics.
        let (old_instruction, analysis) = consumer.definition.instruction();
        let mut mapped_instruction = [0u8; UPROBE_INSN_COPY_SIZE];
        read_user_instruction(
            &inner.user_mapper.utable,
            probe_vaddr,
            &mut mapped_instruction[..analysis.insn_len],
        )?;
        if mapped_instruction[..analysis.insn_len] != old_instruction[..analysis.insn_len] {
            return Err(SystemError::EINVAL);
        }
        (old_instruction, analysis, None)
    };

    // ── Step 2: 确保 XOL 区存在 + 分配 slot ──
    let xol_lease = if let Some(site) = existing_site.as_ref() {
        site.xol_lease.clone()
    } else {
        ensure_xol_and_alloc_slot(mm, &mut inner)?
    };
    let xol_slot_offset = xol_lease.offset();

    // ── P2：注册时预填 XOL slot + 验证 RIP-relative 位移（fail-fast）──
    // slot_vaddr = xol_page_base + slot_offset 此时已知，立即用真实地址调
    // build_xol_slot：位移溢出→EINVAL（注册失败，不引入会在命中时 panic 的探针）；
    // 成功→slot 内容写入物理页，命中时（#BP handler）slot 已就绪、直接 rip→slot。
    if existing_site.is_none() {
        let (slot_vaddr, page_paddr) = { (xol_lease.slot_vaddr(), xol_lease.page_paddr()) };

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
            return Err(SystemError::EINVAL);
        }

        // 写入 XOL slot 物理页（复刻 batch3 fill_xol_slot / patch_byte_in_phys 写法）。
        let kva = unsafe { MMArch::phys_2_virt(page_paddr) }.ok_or(SystemError::EFAULT)?;
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

    let point = Arc::new(point);

    let site = existing_site.unwrap_or_else(|| {
        Arc::new(UprobeSite {
            definition: consumer.definition.clone(),
            probe_vaddr,
            mapping: Arc::downgrade(&vma),
            mapping_state_seq: vma.state_seq(),
            xol_lease: xol_lease.clone(),
            participants: RwLock::new(Arc::new(Vec::new())),
            state: AtomicU8::new(UprobeSiteState::Prepared as u8),
        })
    });
    let entry = Arc::new(RwLock::new(UprobeInstance {
        point,
        insn_analysis: analysis,
        consumer_id,
        xol_lease: xol_lease.clone(),
        site: site.clone(),
        consumer: consumer.clone(),
    }));

    // ── Step 4: 插入 uprobe_list（表项在 0xcc 发布前就绪 — F6）──
    {
        let mut list = mm.uprobe_list.lock_irqsave();
        list.entry(probe_vaddr).or_default().push(entry.clone());
    }
    // Publish the weak reverse index before building the runtime snapshot so
    // a concurrent enable/disable/SET_BPF cannot miss this in-flight site.
    consumer.remember_site(mm, probe_vaddr, &site);
    rebuild_site_participants(mm, probe_vaddr, &site);

    // ── Step 5: 安装 0xcc 断点页 ──
    let first_site = site.state() == UprobeSiteState::Prepared;
    if first_site {
        // hit table 已发布后才允许暴露 0xcc；Armed 在 PTE 提交之前发布。
        site.state
            .store(UprobeSiteState::Armed as u8, Ordering::Release);
    }
    let install_result = if first_site {
        install_breakpoint_page(mm, &mut inner, &vma, page_base_addr, page_offset)
    } else {
        Ok(())
    };
    if let Err(e) = install_result {
        // 回滚：移除表项 + 释放 slot
        {
            let mut list = mm.uprobe_list.lock_irqsave();
            if let Some(entries) = list.get_mut(&probe_vaddr) {
                entries.retain(|x| !Arc::ptr_eq(x, &entry));
            }
        }
        site.state
            .store(UprobeSiteState::Dead as u8, Ordering::Release);
        return Err(e);
    }

    drop(inner);
    Ok(Some(UprobeHandle {
        mm: Arc::downgrade(mm),
        probe_vaddr,
        entry: Some(entry),
    }))
}

// ──────────────────────── 内部实现 ────────────────────────

/// 从目标 mm 的页表读取 probe_vaddr 处的指令字节（最多 16 字节）。
///
/// `PageMapper::translate` 直接 walk 物理页表，不需要目标 mm 的 CR3 上下文，
/// 因此可跨进程读取。若页未 present 返回 `EFAULT`。
fn read_user_instruction(
    mapper: &PageMapper,
    probe_vaddr: usize,
    output: &mut [u8],
) -> Result<(), SystemError> {
    let mut copied = 0;
    while copied < output.len() {
        let vaddr = probe_vaddr.checked_add(copied).ok_or(SystemError::EFAULT)?;
        let page_offset = vaddr & (MMArch::PAGE_SIZE - 1);
        let (paddr, _flags) = mapper
            .translate(VirtAddr::new(vaddr))
            .ok_or(SystemError::EFAULT)?;
        let kva = unsafe { MMArch::phys_2_virt(paddr) }.ok_or(SystemError::EFAULT)?;
        let count = (output.len() - copied).min(MMArch::PAGE_SIZE - page_offset);
        unsafe {
            core::ptr::copy_nonoverlapping(
                (kva.data() + page_offset) as *const u8,
                output[copied..].as_mut_ptr(),
                count,
            );
        }
        copied += count;
    }
    Ok(())
}

/// 确保 mm 有 XOL 区，并分配一个 slot，返回 slot 在页内偏移。
fn ensure_xol_and_alloc_slot(
    mm: &Arc<AddressSpace>,
    inner: &mut RwSemWriteGuard<'_, InnerAddressSpace>,
) -> Result<Arc<XolSlotLease>, SystemError> {
    // 快速路径：XOL 已存在 → 直接分配
    {
        let guard = mm.xol_area.lock_irqsave();
        if let Some(area) = guard.as_ref() {
            return area.alloc_slot().map(Arc::new).ok_or(SystemError::ENOMEM);
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

    // XOL is kernel-owned execution state, not an ordinary anonymous mapping.
    // It must never be copied into a child mm or expanded/remapped as user data.
    // VM_IO also makes MADV_DOFORK reject attempts to clear VM_DONTCOPY, matching
    // the invariant that every mm owns exactly one independently managed XOL area.
    let xol_vma = inner
        .mappings
        .contains(page.virt_address())
        .ok_or(SystemError::EFAULT)?;
    {
        let mut guard = xol_vma.lock();
        let special =
            VmFlags::VM_DONTCOPY | VmFlags::VM_IO | VmFlags::VM_DONTEXPAND | VmFlags::VM_DONTDUMP;
        let flags = *guard.vm_flags() | special;
        guard.set_vm_flags(flags);
    }

    // 获取 XOL 页物理地址（供 batch3 关中断路径写 slot 内容）
    let page_paddr = inner
        .user_mapper
        .utable
        .translate(page.virt_address())
        .map(|(pa, _)| pa)
        .ok_or(SystemError::EFAULT)?;

    let owned_page = {
        let mut pm = page_manager_lock();
        pm.get(&page_paddr).ok_or(SystemError::EFAULT)?
    };
    let area = Arc::new(XolArea {
        page_base: page.virt_address(),
        page_paddr,
        _page: owned_page,
        generation: NEXT_XOL_GENERATION.fetch_add(1, Ordering::Relaxed),
        slot_bitmap: SpinLock::new([0u64; XOL_BITMAP_WORDS]),
    });
    let lease = area.alloc_slot().map(Arc::new).ok_or(SystemError::ENOMEM)?;

    let mut guard = mm.xol_area.lock_irqsave();
    if guard.is_none() {
        *guard = Some(area);
    } else {
        // 因调用方持有 inner.write()，同 mm 的注册是串行的，此分支理论上不可达。
        // TODO: [stage2] unmap 冗余的 XOL 页避免泄漏
        return guard
            .as_ref()
            .unwrap()
            .alloc_slot()
            .map(Arc::new)
            .ok_or(SystemError::ENOMEM);
    }
    Ok(lease)
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
        {
            let _pt_edit = mm.page_table_edit();
            let mapper = &mut inner.user_mapper.utable;
            let (paddr, _) = mapper.translate(address).ok_or(SystemError::EFAULT)?;
            let kva = unsafe { MMArch::phys_2_virt(paddr) }.ok_or(SystemError::EFAULT)?;
            let mut pb = mm.uprobe_page_state.lock_irqsave();
            let state = pb.get_mut(&page_base_addr).ok_or(SystemError::EFAULT)?;
            unsafe {
                core::ptr::write_volatile((kva.data() + page_offset) as *mut u8, 0xcc);
            }
            state.refcount += 1;
        }

        // This branch edits an already mapped physical page, so there is no
        // PTE replacement to serialize instruction fetch on remote CPUs.  A
        // synchronous mm shootdown is the publication point for the new INT3.
        // Do not wait for the IPI while holding page_table_edit or page-state.
        mm.flush_tlb_range(address, end, MMArch::PAGE_SHIFT as u8, false);
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
    let (site, consumer_id) = {
        let inst = entry.read();
        (inst.site.clone(), inst.consumer_id)
    };
    uprobe_unregister_consumer_from_site(mm, probe_vaddr, &site, consumer_id);
}

fn uprobe_unregister_consumer_from_site(
    mm: &Arc<AddressSpace>,
    probe_vaddr: usize,
    site: &Arc<UprobeSite>,
    consumer_id: u64,
) {
    // Registration publishes memberships under mm.write -> uprobe_list.
    // Use the same order so the atomic remove/last decision cannot be
    // invalidated by a new same-address consumer before teardown commits.
    let mut inner = mm.write();
    // Remove a non-last membership while holding the table lock. If this is
    // the last membership, leave it published until the teardown owner has
    // restored the opcode. Two concurrent removals can therefore never both
    // decide they are non-last and strand an armed, ownerless breakpoint.
    let (last_consumer, orig_first_byte) = {
        let mut list = mm.uprobe_list.lock_irqsave();
        let Some(entries) = list.get_mut(&probe_vaddr) else {
            return;
        };
        let Some(remove_index) = entries.iter().position(|entry| {
            let inst = entry.read();
            inst.consumer_id == consumer_id && Arc::ptr_eq(&inst.site, site)
        }) else {
            return;
        };
        let last = entries
            .iter()
            .filter(|entry| Arc::ptr_eq(&entry.read().site, site))
            .count()
            == 1;
        if !last {
            entries.remove(remove_index);
        }
        (last, site.definition.old_instruction[0])
    };

    if !last_consumer {
        rebuild_site_participants(mm, probe_vaddr, site);
        return;
    }

    let page_base_addr = probe_vaddr & !(MMArch::PAGE_SIZE - 1);
    let page_offset = probe_vaddr & (MMArch::PAGE_SIZE - 1);
    if last_consumer {
        if site
            .state
            .compare_exchange(
                UprobeSiteState::Armed as u8,
                UprobeSiteState::Disarming as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return;
        }
        *site.participants.write() = Arc::new(Vec::new());
        // Disarming 期间表项仍可命中；先恢复指令，再撤销 hit table。
        if site_mapping_still_matches(&inner, site)
            && restore_breakpoint_byte(mm, &mut inner, page_base_addr, page_offset, orig_first_byte)
        {
            // The table remains visible while the original opcode is restored.
            // A synchronous shootdown is also a CPU rendezvous: a CPU which
            // already executed INT3 cannot acknowledge it until its irq-off
            // #BP handler has acquired the table and its XOL execution guard.
            mm.flush_tlb_range(
                VirtAddr::new(page_base_addr),
                VirtAddr::new(page_base_addr + MMArch::PAGE_SIZE),
                MMArch::PAGE_SHIFT as u8,
                false,
            );
        }
    }
    {
        let mut list = mm.uprobe_list.lock_irqsave();
        if let Some(entries) = list.get_mut(&probe_vaddr) {
            entries.retain(|entry| {
                let inst = entry.read();
                !(inst.consumer_id == consumer_id && Arc::ptr_eq(&inst.site, site))
            });
            if entries.is_empty() {
                list.remove(&probe_vaddr);
            }
        }
    }
    if last_consumer {
        site.state
            .store(UprobeSiteState::Dead as u8, Ordering::Release);
        let mut pb = mm.uprobe_page_state.lock_irqsave();
        if let Some(state) = pb.get_mut(&page_base_addr) {
            state.refcount = state.refcount.saturating_sub(1);
            if state.refcount == 0 {
                pb.remove(&page_base_addr);
            }
        }
    }
}

fn rebuild_site_participants(mm: &AddressSpace, probe_vaddr: usize, site: &Arc<UprobeSite>) {
    // Keep collection and publication in the same membership critical
    // section. Otherwise an older refresh can publish after a newer
    // SET_BPF/enable/disable/close refresh and resurrect stale callbacks.
    let list = mm.uprobe_list.lock_irqsave();
    let participants = list
        .get(&probe_vaddr)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let instance = entry.read();
            Arc::ptr_eq(&instance.site, site)
                .then(|| instance.consumer.runtime_snapshot())
                .flatten()
        })
        .collect();
    // Publish while membership is still serialized, but defer destruction of
    // the old callbacks until after the IRQ-off hit-table lock is released.
    let old_participants =
        core::mem::replace(&mut *site.participants.write(), Arc::new(participants));
    drop(list);
    drop(old_participants);
}

fn refresh_consumer_sites(consumer: &UprobeConsumer) {
    let installed: Vec<_> = consumer
        .sites
        .lock_irqsave()
        .iter()
        .filter_map(|installed| {
            Some((
                installed.mm.upgrade()?,
                installed.vaddr,
                installed.site.upgrade()?,
            ))
        })
        .collect();
    for (mm, vaddr, site) in installed {
        if site.state() != UprobeSiteState::Dead {
            rebuild_site_participants(&mm, vaddr, &site);
        }
    }
}

/// 注销前按 Linux `register_for_each_vma()` 的方式在 mm 写锁下重验映射身份，
/// 防止 munmap 后同一虚址被无关文件复用时写坏新映射。
fn site_mapping_still_matches(inner: &InnerAddressSpace, site: &UprobeSite) -> bool {
    let Some(vma) = inner.mappings.contains(VirtAddr::new(site.probe_vaddr)) else {
        return false;
    };
    let Some(original_vma) = site.mapping.upgrade() else {
        return false;
    };
    if !Arc::ptr_eq(&vma, &original_vma) || vma.state_seq() != site.mapping_state_seq {
        return false;
    }
    let guard = vma.lock();
    let Some(file) = guard.vm_file() else {
        return false;
    };
    let Some(pgoff) = guard.backing_page_offset() else {
        return false;
    };
    let Some(delta) = site.probe_vaddr.checked_sub(guard.region().start().data()) else {
        return false;
    };
    let Some(offset) = pgoff
        .checked_mul(MMArch::PAGE_SIZE)
        .and_then(|base| base.checked_add(delta))
    else {
        return false;
    };
    site.definition.matches_inode(&file.inode()) && offset == site.definition.offset()
}

/// 在**当前映射页**上恢复断点原字节（评审 R8）。
///
/// 经 `translate` 取当前 paddr（可能是断点安装时的 COW 副本，也可能是程序
/// 写缺页二次 COW 后的页），直接写回原首字节。不交换页映射——页上其他字节
/// 的任何程序写入都保留。无 PTE 变更 → 无需 TLB flush（TLB 缓存翻译而非
/// 内容；跨修改代码的串行化由 #BP 中断返回后的取指重取保证）。
fn restore_breakpoint_byte(
    mm: &Arc<AddressSpace>,
    inner: &mut InnerAddressSpace,
    page_base_addr: usize,
    page_offset: usize,
    orig_first_byte: u8,
) -> bool {
    let _pt_edit = mm.page_table_edit();
    let mapper = &mut inner.user_mapper.utable;
    if let Some((paddr, _)) = mapper.translate(VirtAddr::new(page_base_addr)) {
        if let Some(kva) = unsafe { MMArch::phys_2_virt(paddr) } {
            unsafe {
                core::ptr::write_volatile((kva.data() + page_offset) as *mut u8, orig_first_byte);
            }
            return true;
        }
    }
    // 页已被 munmap（translate 失败）：无需恢复。
    false
}

/// Remove every uprobe site whose instruction lies in `region` before a VMA/PTE
/// mutation commits.  The caller owns `AddressSpace::write()`, so mapping
/// identity and the restored byte are checked against one stable VMA view.
///
/// The XOL VMA is kernel-owned and immutable while the address space lives.
/// User VMA operations which overlap it are rejected before any probe byte or
/// mapping is changed.
pub(crate) fn uprobe_disarm_range_locked(
    mm: &Arc<AddressSpace>,
    inner: &mut InnerAddressSpace,
    region: VirtRegion,
) -> Result<(), SystemError> {
    let overlaps_xol = {
        let area = mm.xol_area.lock_irqsave();
        area.as_ref().is_some_and(|area| {
            let xol = VirtRegion::new(area.page_base(), MMArch::PAGE_SIZE);
            xol.collide(&region)
        })
    };
    // This VM_IO|VM_DONTEXPAND mapping is an execution trampoline, not a
    // user-remappable allocation. Rejecting overlap avoids both waiting under
    // mm.write() and a rollback window in which all probes are withdrawn.
    if overlaps_xol {
        return Err(SystemError::EBUSY);
    }

    let candidate_start = region
        .start()
        .data()
        .saturating_sub(UPROBE_INSN_COPY_SIZE - 1);
    let targets: Vec<(usize, Arc<UprobeSite>, u8)> = {
        let list = mm.uprobe_list.lock_irqsave();
        list.range(candidate_start..region.end().data())
            .filter_map(|(vaddr, entries)| {
                let entry = entries.first()?.read();
                let instruction_end = vaddr.checked_add(entry.insn_analysis.insn_len)?;
                if *vaddr >= region.end().data() || instruction_end <= region.start().data() {
                    return None;
                }
                let old = entry.point.old_instruction[0];
                Some((*vaddr, entry.site.clone(), old))
            })
            .collect()
    };

    let mut disarmed = Vec::new();
    for (probe_vaddr, site, old_byte) in targets {
        if site
            .state
            .compare_exchange(
                UprobeSiteState::Armed as u8,
                UprobeSiteState::Disarming as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            continue;
        }

        // Keep the hit-table entry visible until the original byte is restored.
        // A concurrent old #BP can therefore still find the site and execute its
        // strongly held XOL lease. VMA withdrawal does not suppress callbacks
        // for an instruction which already trapped before the mapping change.
        if site_mapping_still_matches(inner, &site) {
            let page_base = probe_vaddr & !(MMArch::PAGE_SIZE - 1);
            let page_offset = probe_vaddr & (MMArch::PAGE_SIZE - 1);
            restore_breakpoint_byte(mm, inner, page_base, page_offset, old_byte);
        }
        disarmed.push((probe_vaddr, site, old_byte));
    }

    if !disarmed.is_empty() {
        // Besides invalidating translations, this synchronous shootdown is the
        // grace point for CPUs which executed INT3 before the bytes above were
        // restored. Such a CPU runs #BP with interrupts disabled and cannot
        // acknowledge the IPI until it has acquired the hit-table entry and
        // its strongly held XOL slot lease.
        // The acknowledgement, not the flushed address coverage, provides
        // the grace point. Flush one affected page here; the VMA operation
        // performs its own range-wide TLB invalidation when it commits.
        let rendezvous_page = disarmed[0].0 & !(MMArch::PAGE_SIZE - 1);
        mm.flush_tlb_range(
            VirtAddr::new(rendezvous_page),
            VirtAddr::new(rendezvous_page + MMArch::PAGE_SIZE),
            MMArch::PAGE_SHIFT as u8,
            false,
        );
    }

    for (probe_vaddr, site, _) in disarmed {
        *site.participants.write() = Arc::new(Vec::new());
        {
            let mut list = mm.uprobe_list.lock_irqsave();
            // Conditional removal prevents an old lifecycle delta from deleting
            // a newer site installed at the same virtual address.
            if list.get(&probe_vaddr).is_some_and(|entries| {
                entries
                    .first()
                    .is_some_and(|entry| Arc::ptr_eq(&entry.read().site, &site))
            }) {
                list.remove(&probe_vaddr);
            }
        }
        site.state
            .store(UprobeSiteState::Dead as u8, Ordering::Release);
        let page_base = probe_vaddr & !(MMArch::PAGE_SIZE - 1);
        let mut pages = mm.uprobe_page_state.lock_irqsave();
        if let Some(state) = pages.get_mut(&page_base) {
            debug_assert!(state.refcount != 0, "uprobe page refcount underflow");
            if state.refcount <= 1 {
                pages.remove(&page_base);
            } else {
                state.refcount -= 1;
            }
        } else {
            debug_assert!(false, "armed uprobe site without page state");
        }
    }
    Ok(())
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
    pub definition: Arc<UprobeDefinition>,
    pub scope: UprobeConsumerScope,
    pub pre_handler: fn(&dyn ProbeArgs),
    pub post_handler: fn(&dyn ProbeArgs),
    pub event_callback: Option<Arc<dyn uprobe::CallBackFunc>>,
    pub enabled: bool,
}

/// 全局注册表：inode id → 文件偏移 → （消费者 id，回调）。
/// 注册表值类型：某（inode, offset）上的消费者列表。
type ConsumerList = Vec<Arc<UprobeConsumer>>;
/// 注册表类型：inode id → （文件偏移 → 消费者列表）。
type RegistryMap = BTreeMap<usize, BTreeMap<usize, ConsumerList>>;

static UPROBE_REGISTRY: SpinLock<RegistryMap> = SpinLock::new(BTreeMap::new());

static NEXT_CONSUMER_ID: AtomicU64 = AtomicU64::new(1);
static ACTIVE_UPROBE_CONSUMERS: AtomicUsize = AtomicUsize::new(0);
static NEXT_TASK_SCOPE_ID: AtomicU64 = AtomicU64::new(1);
static UPROBE_TASK_SCOPES: SpinLock<BTreeMap<u64, Weak<ProcessControlBlock>>> =
    SpinLock::new(BTreeMap::new());
static UPROBE_DEFINITIONS: SpinLock<BTreeMap<(usize, usize), Weak<UprobeDefinition>>> =
    SpinLock::new(BTreeMap::new());

fn uprobe_registry_is_empty() -> bool {
    ACTIVE_UPROBE_CONSUMERS.load(Ordering::Acquire) == 0
}

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
    debug_assert_eq!(inode_id, reg.definition.inode_id());
    debug_assert_eq!(offset, reg.definition.offset());
    let consumer = UprobeConsumer::new(
        consumer_id,
        reg.definition.clone(),
        match &reg.scope {
            UprobeConsumerScope::Task(task) => UprobeConsumerScope::Task(task.clone()),
            UprobeConsumerScope::SystemWideAuthorized => UprobeConsumerScope::SystemWideAuthorized,
        },
        UprobeConsumerRuntime {
            pre_handler: reg.pre_handler,
            post_handler: reg.post_handler,
            event_callback: reg.event_callback.clone(),
            enabled: reg.enabled,
        },
    );
    uprobe_registry_add_consumer(consumer);
}

pub fn uprobe_registry_add_consumer(consumer: Arc<UprobeConsumer>) {
    if consumer.enabled.load(Ordering::Acquire) {
        ACTIVE_UPROBE_CONSUMERS.fetch_add(1, Ordering::AcqRel);
    }
    let mut r = UPROBE_REGISTRY.lock_irqsave();
    r.entry(consumer.definition.inode_key)
        .or_default()
        .entry(consumer.definition.offset())
        .or_default()
        .push(consumer);
}

fn registry_consumer(consumer_id: u64) -> Option<Arc<UprobeConsumer>> {
    let r = UPROBE_REGISTRY.lock_irqsave();
    r.values()
        .flat_map(|offsets| offsets.values())
        .flatten()
        .find(|consumer| consumer.id == consumer_id)
        .cloned()
}

/// 更新某消费者的 BPF 事件回调（PERF_EVENT_IOC_SET_BPF 时调用）。
/// 迟到安装的实例据此取得与直接安装一致的回调。
pub fn uprobe_registry_attach_callback(
    consumer_id: u64,
    cb: Arc<dyn uprobe::CallBackFunc>,
) -> Result<(), SystemError> {
    // Do not take consumer locks while holding the registry spinlock.
    let consumer = registry_consumer(consumer_id).ok_or(SystemError::ENOENT)?;
    let _lifecycle = consumer.lifecycle.lock();
    if consumer.closing.load(Ordering::Acquire) {
        return Err(SystemError::ENOENT);
    }
    {
        let mut runtime = consumer.runtime.write();
        if runtime.event_callback.is_some() {
            return Err(SystemError::EEXIST);
        }
        runtime.event_callback = Some(cb);
    }
    refresh_consumer_sites(&consumer);
    Ok(())
}

/// Update the single consumer-level enable state used by both already armed
/// and later-installed sites.
pub fn uprobe_registry_set_enabled(consumer_id: u64, enabled: bool) -> Result<(), SystemError> {
    let consumer = registry_consumer(consumer_id).ok_or(SystemError::ENOENT)?;
    let _lifecycle = consumer.lifecycle.lock();
    if consumer.closing.load(Ordering::Acquire) {
        return Err(SystemError::ENOENT);
    }
    if consumer.enabled.load(Ordering::Acquire) == enabled {
        return Ok(());
    }
    if enabled {
        consumer.runtime.write().enabled = true;
        if !consumer.enabled.swap(true, Ordering::AcqRel) {
            ACTIVE_UPROBE_CONSUMERS.fetch_add(1, Ordering::AcqRel);
        }
        if let Err(e) = apply_consumer_to_existing_mappings(&consumer) {
            if consumer.enabled.swap(false, Ordering::AcqRel) {
                ACTIVE_UPROBE_CONSUMERS.fetch_sub(1, Ordering::AcqRel);
            }
            consumer.runtime.write().enabled = false;
            consumer
                .inflight_wait
                .wait_until(|| (consumer.inflight.load(Ordering::Acquire) == 0).then_some(()));
            detach_consumer_sites(&consumer);
            return Err(e);
        }
    } else {
        if consumer.enabled.swap(false, Ordering::AcqRel) {
            ACTIVE_UPROBE_CONSUMERS.fetch_sub(1, Ordering::AcqRel);
        }
        consumer.runtime.write().enabled = false;
        consumer
            .inflight_wait
            .wait_until(|| (consumer.inflight.load(Ordering::Acquire) == 0).then_some(()));
        detach_consumer_sites(&consumer);
    }
    Ok(())
}

fn detach_consumer_sites(consumer: &UprobeConsumer) {
    let installed = core::mem::take(&mut *consumer.sites.lock_irqsave());
    for installed in installed {
        if let (Some(mm), Some(site)) = (installed.mm.upgrade(), installed.site.upgrade()) {
            uprobe_unregister_consumer_from_site(&mm, installed.vaddr, &site, consumer.id);
        }
    }
}

fn apply_consumer_to_existing_mappings(consumer: &Arc<UprobeConsumer>) -> Result<(), SystemError> {
    if let UprobeConsumerScope::Task(target) = &consumer.scope {
        let Some(mm) = target.target_mm() else {
            return Ok(());
        };
        return apply_consumer_to_mm(consumer, &mm);
    }

    let page_cache = consumer
        .definition
        .inode()
        .page_cache()
        .ok_or(SystemError::EINVAL)?;
    for vma in page_cache.collect_file_vmas() {
        let mapping = {
            let guard = vma.lock();
            let Some(mm) = guard.address_space().and_then(|owner| owner.upgrade()) else {
                continue;
            };
            let Some(pgoff) = guard.backing_page_offset() else {
                continue;
            };
            let Some(file) = guard.vm_file() else {
                continue;
            };
            (mm, file, *guard.region(), pgoff)
        };
        uprobe_apply_to_new_vma_inner(
            &mapping.0,
            &mapping.1,
            mapping.2.start().data(),
            mapping.2.size(),
            mapping.3 << MMArch::PAGE_SHIFT,
            true,
            Some(consumer.id),
        )?;
    }
    Ok(())
}

fn apply_consumer_to_mm(
    consumer: &Arc<UprobeConsumer>,
    mm: &Arc<AddressSpace>,
) -> Result<(), SystemError> {
    let all = VirtRegion::new(VirtAddr::new(0), MMArch::USER_END_VADDR.data());
    let definition_offset = consumer.definition.offset();
    for (file, start, size, file_start) in collect_file_vma_snapshot(mm, all) {
        if !consumer.definition.matches_inode(&file.inode()) {
            continue;
        }
        let Some(file_end) = file_start.checked_add(size) else {
            return Err(SystemError::EINVAL);
        };
        if definition_offset < file_start || definition_offset >= file_end {
            continue;
        }
        uprobe_apply_to_new_vma_inner(mm, &file, start, size, file_start, true, Some(consumer.id))?;
    }
    Ok(())
}
/// 消费者关闭：移除注册表项 + drop 迟到句柄（逐 mm 注销）。
pub fn uprobe_registry_remove_consumer(consumer_id: u64) {
    let removed = {
        let mut r = UPROBE_REGISTRY.lock_irqsave();
        let mut removed = None;
        for (_, offsets) in r.iter_mut() {
            for (_, consumers) in offsets.iter_mut() {
                consumers.retain(|consumer| {
                    if consumer.id == consumer_id {
                        consumer.closing.store(true, Ordering::Release);
                        if consumer.enabled.swap(false, Ordering::AcqRel) {
                            ACTIVE_UPROBE_CONSUMERS.fetch_sub(1, Ordering::AcqRel);
                        }
                        removed = Some(consumer.clone());
                        false
                    } else {
                        true
                    }
                });
            }
            offsets.retain(|_, consumers| !consumers.is_empty());
        }
        r.retain(|_, offsets| !offsets.is_empty());
        removed
    };
    let Some(consumer) = removed else { return };
    consumer
        .inflight_wait
        .wait_until(|| (consumer.inflight.load(Ordering::Acquire) == 0).then_some(()));
    let sites = core::mem::take(&mut *consumer.sites.lock_irqsave());
    for installed in sites {
        if let (Some(mm), Some(site)) = (installed.mm.upgrade(), installed.site.upgrade()) {
            uprobe_unregister_consumer_from_site(&mm, installed.vaddr, &site, consumer.id);
        }
    }
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
    let _ = uprobe_apply_to_new_vma_inner(
        mm,
        file,
        region_start,
        region_size,
        file_start_byte,
        false,
        None,
    );
}

fn uprobe_apply_to_new_vma_inner(
    mm: &Arc<AddressSpace>,
    file: &Arc<crate::filesystem::vfs::file::File>,
    region_start: usize,
    region_size: usize,
    file_start_byte: usize,
    strict: bool,
    only_consumer_id: Option<u64>,
) -> Result<(), SystemError> {
    let inode = file.inode();
    let inode_id = inode.metadata().map(|md| md.inode_id.data()).unwrap_or(0);
    let Some(page_cache) = inode.page_cache() else {
        return Ok(());
    };
    let inode_key = Arc::as_ptr(&page_cache) as usize;
    let region_file_end = file_start_byte
        .checked_add(region_size)
        .ok_or(SystemError::EINVAL)?;
    // 锁内快照：落在新 VMA 文件区间内的消费者列表
    let matches: Vec<(usize, ConsumerList)> = {
        let r = UPROBE_REGISTRY.lock_irqsave();
        let Some(offsets) = r.get(&inode_key) else {
            return Ok(());
        };
        offsets
            .range(file_start_byte..region_file_end)
            .filter_map(|(off, consumers)| {
                let consumers = if let Some(id) = only_consumer_id {
                    consumers
                        .iter()
                        .filter(|consumer| consumer.id == id)
                        .cloned()
                        .collect()
                } else {
                    consumers.clone()
                };
                (!consumers.is_empty()).then_some((*off, consumers))
            })
            .collect()
    };
    if matches.is_empty() {
        return Ok(());
    }

    for (offset, consumers) in matches {
        let probe_vaddr = region_start
            .checked_add(offset - file_start_byte)
            .ok_or(SystemError::EINVAL)?;
        for consumer in consumers {
            if !consumer.scope.permits(mm) || consumer.closing.load(Ordering::Acquire) {
                continue;
            }
            // Ordinary file mmap is lazy. Fault in the one or two pages which
            // contain the complete instruction, pinning each retry to the VMA
            // identity observed here so MAP_FIXED cannot redirect installation.
            let expected_mapping = {
                let inner = mm.read();
                capture_probe_mapping(&inner, &consumer.definition, probe_vaddr)
            };
            let Some(expected_mapping) = expected_mapping else {
                // An incomplete, non-executable, non-contiguous, shared, or
                // writable alias is not an install failure. Keep the consumer
                // registered for a later eligible mapping.
                continue;
            };
            let mut population_error = None;
            for part in &expected_mapping.parts {
                let page_start = part.region.start().data() & !(MMArch::PAGE_SIZE - 1);
                let Some(page_end) = part
                    .region
                    .end()
                    .data()
                    .checked_add(MMArch::PAGE_SIZE - 1)
                    .map(|end| end & !(MMArch::PAGE_SIZE - 1))
                else {
                    population_error = Some(SystemError::EINVAL);
                    break;
                };
                if let Err(e) = mm.populate_range_post_commit(
                    VirtAddr::new(page_start),
                    page_end - page_start,
                    true,
                    false,
                    Some(part.vma.clone()),
                ) {
                    population_error = Some(e);
                    break;
                }
            }
            if let Some(e) = population_error {
                log::debug!(
                    "uprobe fault-in {:x}+{:#x} in new vma failed: {:?}",
                    inode_id,
                    offset,
                    e
                );
                if strict {
                    return Err(e);
                }
                continue;
            }
            match uprobe_register(
                mm,
                probe_vaddr,
                noop_handler,
                noop_handler,
                &consumer,
                &expected_mapping,
            ) {
                Ok(Some(handle)) => {
                    handle.persist();
                }
                Ok(None) => continue,
                Err(e) => {
                    log::debug!(
                        "uprobe late-apply {:x}+{:#x} in new vma failed: {:?}",
                        inode_id,
                        offset,
                        e
                    );
                    // ENOENT is a consumer closing concurrently and therefore a
                    // successful absence, not a fork transaction failure.
                    if strict && e != SystemError::ENOENT {
                        return Err(e);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Re-evaluate all file VMAs intersecting `region` after a committed VMA
/// operation.  The VMA snapshot is owned and the address-space lock is dropped
/// before fault-in/registration, preserving the registry -> mm lock boundary.
fn collect_file_vma_snapshot(
    mm: &Arc<AddressSpace>,
    region: VirtRegion,
) -> Vec<(Arc<crate::filesystem::vfs::file::File>, usize, usize, usize)> {
    {
        let inner = mm.read();
        inner
            .mappings
            .conflicts(region)
            .into_iter()
            .filter_map(|vma| {
                let guard = vma.lock();
                let file = guard.vm_file()?;
                let pgoff = guard.backing_page_offset()?;
                let vma_region = *guard.region();
                let intersection = vma_region.intersect(&region)?;
                let file_start = pgoff
                    .checked_mul(MMArch::PAGE_SIZE)?
                    .checked_add(intersection.start().data() - vma_region.start().data())?;
                Some((
                    file,
                    intersection.start().data(),
                    intersection.size(),
                    file_start,
                ))
            })
            .collect()
    }
}

pub(crate) fn uprobe_apply_to_range(mm: &Arc<AddressSpace>, region: VirtRegion) {
    if uprobe_registry_is_empty() {
        mm.uprobe_needs_full_reapply.store(false, Ordering::Release);
        return;
    }
    let full_reapply = mm.uprobe_needs_full_reapply.swap(false, Ordering::AcqRel);
    let region = if full_reapply {
        VirtRegion::new(VirtAddr::new(0), MMArch::USER_END_VADDR.data())
    } else {
        // A variable-length x86 instruction can start on the preceding page
        // or VMA and overlap this mutation with only its tail. Reconsider the
        // maximum possible prefix so a disarmed cross-boundary site can be
        // restored only after its complete mapping is valid again.
        let start = region
            .start()
            .data()
            .saturating_sub(UPROBE_INSN_COPY_SIZE - 1);
        VirtRegion::new(VirtAddr::new(start), region.end().data() - start)
    };
    let mut retry_full = false;
    for (file, start, size, offset) in collect_file_vma_snapshot(mm, region) {
        if full_reapply {
            if uprobe_apply_to_new_vma_inner(mm, &file, start, size, offset, true, None).is_err() {
                retry_full = true;
            }
        } else {
            uprobe_apply_to_new_vma(mm, &file, start, size, offset);
        }
    }
    if retry_full {
        mm.uprobe_needs_full_reapply.store(true, Ordering::Release);
    }
}

pub(crate) fn uprobe_apply_to_all_vmas(mm: &Arc<AddressSpace>) {
    uprobe_apply_to_range(
        mm,
        VirtRegion::new(VirtAddr::new(0), MMArch::USER_END_VADDR.data()),
    );
}

/// Reconcile only the source and destination touched by mremap. The first
/// range automatically widens to the whole mm when an XOL VMA was invalidated.
pub(crate) fn uprobe_apply_to_mremap_ranges(
    mm: &Arc<AddressSpace>,
    old_vaddr: VirtAddr,
    old_len: usize,
    new_vaddr: VirtAddr,
    new_len: usize,
) {
    if old_len != 0 {
        uprobe_apply_to_range(mm, VirtRegion::new(old_vaddr, old_len));
    }
    if new_len != 0 && (new_vaddr != old_vaddr || new_len != old_len) {
        uprobe_apply_to_range(mm, VirtRegion::new(new_vaddr, new_len));
    }
}

fn uprobe_apply_to_all_vmas_strict(mm: &Arc<AddressSpace>) -> Result<(), SystemError> {
    let all = VirtRegion::new(VirtAddr::new(0), MMArch::USER_END_VADDR.data());
    for (file, start, size, offset) in collect_file_vma_snapshot(mm, all) {
        uprobe_apply_to_new_vma_inner(mm, &file, start, size, offset, true, None)?;
    }
    Ok(())
}

/// Restore breakpoint bytes inherited through fork COW before the child mm is
/// exposed without its write lock.
///
/// Child file VMAs are already linked into file-rmap at this point. Keeping
/// their mm write-locked while replacing inherited breakpoint pages prevents
/// a concurrent system-wide registry scan from observing `0xcc` without the
/// corresponding child hit-table entry.
pub(crate) fn fork_restore_inherited_uprobes_locked(
    parent_mm: &Arc<AddressSpace>,
    child_mm: &Arc<AddressSpace>,
    inner: &mut InnerAddressSpace,
) -> Result<(), SystemError> {
    // The child initially shares the parent's private breakpoint pages through
    // normal fork COW.  Restore every inherited 0xcc in one private copy per
    // physical page before the child can run.  Afterwards the registry applies
    // only consumers whose scope actually permits the child mm (system-wide in
    // phase 1; task events have inherit=0).
    let snapshot: BTreeMap<usize, Vec<(usize, u8)>> = {
        let list = parent_mm.uprobe_list.lock_irqsave();
        let mut pages = BTreeMap::<usize, Vec<(usize, u8)>>::new();
        for (vaddr, entries) in list.iter() {
            let Some(entry) = entries.first() else {
                continue;
            };
            let old_byte = {
                let entry = entry.read();
                entry.point.old_instruction[0]
            };
            pages
                .entry(*vaddr & !(MMArch::PAGE_SIZE - 1))
                .or_default()
                .push((*vaddr & (MMArch::PAGE_SIZE - 1), old_byte));
        }
        pages
    };
    if snapshot.is_empty() {
        return Ok(());
    }

    for (page_base, patches) in snapshot {
        let address = VirtAddr::new(page_base);
        let Some(vma) = inner.mappings.contains(address) else {
            // A VM_DONTCOPY source VMA intentionally has no child mapping.
            continue;
        };
        let (old_paddr, entry_flags) = inner
            .user_mapper
            .utable
            .translate(address)
            .ok_or(SystemError::EFAULT)?;
        let old_page = {
            let mut pages = page_manager_lock();
            pages.get(&old_paddr).ok_or(SystemError::EFAULT)?
        };
        let new_page = {
            let mapper = &mut inner.user_mapper.utable;
            let mut pages = page_manager_lock();
            pages
                .copy_page_as_normal(&old_paddr, mapper.allocator_mut())
                .map_err(|_| SystemError::ENOMEM)?
        };
        for (offset, byte) in patches {
            patch_byte_in_phys(&new_page, offset, byte)?;
        }
        {
            let _pt_edit = child_mm.page_table_edit();
            let mapper = &mut inner.user_mapper.utable;
            let table = mapper.get_table(address, 0).ok_or(SystemError::EFAULT)?;
            let index = table.index_of(address).ok_or(SystemError::EFAULT)?;
            unsafe {
                table.set_entry(index, PageEntry::new(new_page.phys_address(), entry_flags));
            }
        }
        let vm_locked = vma.lock().vm_flags().contains(VmFlags::VM_LOCKED);
        new_page.write().insert_vma(vma.clone(), vm_locked);
        old_page.write().remove_vma(vma.as_ref());
        InnerAddressSpace::remove_page_unevictable_if_unneeded(&old_page);
        child_mm.flush_tlb_range(
            address,
            VirtAddr::new(page_base + MMArch::PAGE_SIZE),
            MMArch::PAGE_SHIFT as u8,
            false,
        );
    }
    Ok(())
}

/// Replay all currently enabled consumers after fork has published a clean
/// child address space. Concurrent register scans are idempotent because both
/// paths serialize installation with the child mm write lock.
pub fn fork_inherit_uprobes(child_mm: &Arc<AddressSpace>) -> Result<(), SystemError> {
    uprobe_apply_to_all_vmas_strict(child_mm)
}
