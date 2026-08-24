use super::*;

/// Per-address-space uprobe lookup table.
///
/// `control` is the only mutable source of truth. `hit_snapshot` is an
/// immutable RCU publication derived from it, so #BP lookup never serializes
/// CPUs on a shared lock. Writers publish the snapshot before exposing INT3
/// and withdraw it only after opcode restoration plus the TLB rendezvous.
#[derive(Debug)]
pub struct UprobeSiteTable {
    control: Mutex<BTreeMap<usize, Arc<UprobeSite>>>,
    hit_snapshot: crate::rcu::RcuArcSlot<BTreeMap<usize, Arc<UprobeSite>>>,
}

impl UprobeSiteTable {
    pub fn new() -> Self {
        Self {
            control: Mutex::new(BTreeMap::new()),
            hit_snapshot: crate::rcu::RcuArcSlot::new(Arc::new(BTreeMap::new())),
        }
    }

    /// Execute a bounded lookup while RCU pins the immutable map and site.
    /// The callback copies only the IRQ-safe values needed after it returns.
    pub fn with_hit<R>(&self, vaddr: usize, f: impl FnOnce(&UprobeSite) -> R) -> Option<R> {
        self.hit_snapshot
            .with_read(|sites| sites.get(&vaddr).map(|site| f(site)))
    }

    pub(super) fn get(&self, vaddr: usize) -> Option<Arc<UprobeSite>> {
        self.control.lock().get(&vaddr).cloned()
    }

    pub(super) fn range(&self, range: core::ops::Range<usize>) -> Vec<(usize, Arc<UprobeSite>)> {
        self.control
            .lock()
            .range(range)
            .map(|(vaddr, site)| (*vaddr, site.clone()))
            .collect()
    }

    pub(super) fn insert(&self, vaddr: usize, site: Arc<UprobeSite>) -> Option<Arc<UprobeSite>> {
        let (previous, snapshot) = {
            let mut control = self.control.lock();
            let previous = control.insert(vaddr, site);
            (previous, Arc::new(control.clone()))
        };
        self.hit_snapshot.store_deferred(snapshot);
        previous
    }

    pub(super) fn remove_if(&self, vaddr: usize, expected: &Arc<UprobeSite>) -> bool {
        let snapshot = {
            let mut control = self.control.lock();
            if !control
                .get(&vaddr)
                .is_some_and(|site| Arc::ptr_eq(site, expected))
            {
                return false;
            }
            control.remove(&vaddr);
            Arc::new(control.clone())
        };
        self.hit_snapshot.store_deferred(snapshot);
        true
    }

    pub(super) fn take_all(&self) -> BTreeMap<usize, Arc<UprobeSite>> {
        core::mem::take(&mut *self.control.lock())
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
    mapping_lineage_id: u64,
    pub(super) old_instruction: [u8; UPROBE_INSN_COPY_SIZE],
    pub(crate) insn_analysis: InsnAnalysis,
    pub xol_lease: Arc<XolSlotLease>,
    pub(super) members: Mutex<BTreeMap<u64, UprobeSiteMember>>,
    /// Immutable hit-path snapshot. Writers rebuild it in process context;
    /// #BP only clones the Arc and never allocates.
    pub participants: crate::rcu::RcuArcSlot<Vec<UprobeConsumerRuntimeSnapshot>>,
    pub(super) state: AtomicU8,
}

pub(super) struct UprobeSiteMember {
    pub(super) consumer: Arc<UprobeConsumer>,
    target: UprobeConsumerRuntimeSnapshot,
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

impl core::fmt::Debug for UprobeSite {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UprobeSite")
            .field("probe_vaddr", &self.probe_vaddr)
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}

impl core::fmt::Debug for XolArea {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("XolArea")
            .field("page_base", &self.page_base())
            .finish_non_exhaustive()
    }
}

/// 某个页上已安装断点的状态标记。
///
/// 以页基地址（`probe_vaddr & !(PAGE_SIZE-1)`）为键。多个 uprobe 命中同一页时共享
/// 一个 COW 副本，`refcount` 记录活跃断点数。注销在**当前映射页**上恢复字节、
/// 不换页（评审 R8），故此处仅保留计数标记（供安装路径判定「页已私有化」）。
pub(crate) struct UprobePageState {
    /// 活跃断点数。
    pub(super) refcount: usize,
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
    site: Option<Arc<UprobeSite>>,
    consumer_id: u64,
}

impl UprobeHandle {
    /// 把所有权转交给 consumer 的弱 site 索引；用于 mmap/fork 的持久安装。
    pub(super) fn persist(mut self) {
        self.site.take();
    }

    /// Roll back an installation while the caller still owns this mm's write
    /// guard. This closes the task-exec race without recursively acquiring the
    /// same address-space lock through `Drop`.
    pub(super) fn rollback_locked(
        mut self,
        mm: &Arc<AddressSpace>,
        inner: &mut RwSemWriteGuard<'_, InnerAddressSpace>,
    ) {
        if let Some(site) = self.site.take() {
            uprobe_unregister_consumer_from_site_locked(
                mm,
                inner,
                self.probe_vaddr,
                &site,
                self.consumer_id,
            );
        }
    }
}

impl Drop for UprobeHandle {
    fn drop(&mut self) {
        if let Some(site) = self.site.take() {
            if let Some(mm) = self.mm.upgrade() {
                uprobe_unregister_consumer_from_site(
                    &mm,
                    self.probe_vaddr,
                    &site,
                    self.consumer_id,
                );
            }
        }
    }
}

pub(super) struct ExpectedProbeVma {
    pub(super) vma: Weak<LockedVMA>,
    state_seq: u64,
    pub(super) region: VirtRegion,
}

pub(super) struct ExpectedProbeMapping {
    pub(super) parts: Vec<ExpectedProbeVma>,
}

pub(super) fn valid_probe_vma_flags(flags: VmFlags) -> bool {
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

pub(super) fn capture_probe_mapping(
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
/// ## 返回
/// `Ok(UprobeHandle)` 或错误码（`EINVAL`=地址非法/指令不支持，`EFAULT`=页未映射，
/// `ENOMEM`=内存不足/XOL 区满，`EACCES`=VMA 不可执行）。
///
pub(super) fn uprobe_register(
    mm: &Arc<AddressSpace>,
    probe_vaddr: usize,
    consumer: &Arc<UprobeConsumer>,
    expected_mapping: &ExpectedProbeMapping,
) -> Result<Option<UprobeHandle>, SystemError> {
    let result = {
        let mut inner = mm.write();
        uprobe_register_locked(mm, &mut inner, probe_vaddr, consumer, expected_mapping)
    };
    let handle = match result {
        Ok(Some(handle)) => handle,
        other => return other,
    };
    // A successful exec may switch the task's mm while registration waits for
    // or owns the old address-space lock. The exec hook removes installs that
    // completed first; this second check removes an install which published
    // after that scan.
    if !consumer.scope.permits(mm) {
        drop(handle);
        return Ok(None);
    }
    Ok(Some(handle))
}

/// Install one site while the caller already owns this address space's write
/// guard. New-VMA reconciliation uses this entry point before it publishes the
/// mapping outside the write-locked commit boundary.
pub(super) fn uprobe_register_locked(
    mm: &Arc<AddressSpace>,
    inner: &mut RwSemWriteGuard<'_, InnerAddressSpace>,
    probe_vaddr: usize,
    consumer: &Arc<UprobeConsumer>,
    expected_mapping: &ExpectedProbeMapping,
) -> Result<Option<UprobeHandle>, SystemError> {
    let install = consumer.begin_install(mm).ok_or(SystemError::ENOENT)?;
    let consumer_id = consumer.id;

    // ── Step 1: 定位 VMA + 读原指令 + 分析 ──
    let page_base_addr = probe_vaddr & !(MMArch::PAGE_SIZE - 1);
    let page_offset = probe_vaddr & (MMArch::PAGE_SIZE - 1);

    // Fault-in runs without mm.write(), so MAP_FIXED/mprotect can replace any
    // part of a cross-page instruction. Revalidate the complete executable,
    // canonical-file, contiguous-offset mapping chain in this stable view.
    let Some(vma) =
        revalidate_probe_mapping(inner, &consumer.definition, probe_vaddr, expected_mapping)
    else {
        return Ok(None);
    };

    // ── P1：重复注册同一 probe_vaddr 时复用已有指令信息（避免读到 0xcc）──
    // 第二个 consumer 注册同一地址时 PTE 已指向含 0xcc 的 COW 副本，
    // read_user_insn_bytes 会把 0xcc 当原指令。故先查 uprobe_list：若有同址条目，
    // 复用其 old_instruction + insn_analysis（二者对所有同址实例一致），跳过读取。
    let existing_site = mm.uprobe_list.get(probe_vaddr);

    if existing_site
        .as_ref()
        .is_some_and(|site| site.members.lock().contains_key(&consumer_id))
    {
        return Ok(None);
    }

    let (old_instruction, analysis) = if let Some(site) = existing_site.as_ref() {
        if !Arc::ptr_eq(&site.definition, &consumer.definition) {
            return Err(SystemError::EINVAL);
        }
        (site.old_instruction, site.insn_analysis)
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
        (old_instruction, analysis)
    };

    // ── Step 2: 确保 XOL 区存在 + 分配 slot ──
    let xol_lease = if let Some(site) = existing_site.as_ref() {
        site.xol_lease.clone()
    } else {
        ensure_xol_and_alloc_slot(mm, inner)?
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
    let first_site = existing_site.is_none();
    let site = existing_site.unwrap_or_else(|| {
        Arc::new(UprobeSite {
            definition: consumer.definition.clone(),
            probe_vaddr,
            mapping_lineage_id: vma.lineage_id(),
            old_instruction,
            insn_analysis: analysis,
            xol_lease: xol_lease.clone(),
            members: Mutex::new(BTreeMap::new()),
            participants: crate::rcu::RcuArcSlot::new(Arc::new(Vec::new())),
            state: AtomicU8::new(UprobeSiteState::Prepared as u8),
        })
    });

    site.members.lock().insert(
        consumer_id,
        UprobeSiteMember {
            consumer: consumer.clone(),
            target: install.hit_target(),
        },
    );

    // ── Step 4: 插入 uprobe_list（表项在 0xcc 发布前就绪 — F6）──
    if first_site {
        let previous = mm.uprobe_list.insert(probe_vaddr, site.clone());
        debug_assert!(previous.is_none());
    }
    // Publish the weak reverse index before building the runtime snapshot so
    // a concurrent enable/disable/SET_BPF cannot miss this in-flight site.
    consumer.remember_site(mm, probe_vaddr, &site);
    rebuild_site_participants(&site);

    // ── Step 5: 安装 0xcc 断点页 ──
    if first_site {
        // hit table 已发布后才允许暴露 0xcc；Armed 在 PTE 提交之前发布。
        site.state
            .store(UprobeSiteState::Armed as u8, Ordering::Release);
    }
    let install_result = if first_site {
        install_breakpoint_page(mm, inner, &vma, page_base_addr, page_offset)
    } else {
        Ok(())
    };
    if let Err(e) = install_result {
        site.members.lock().remove(&consumer_id);
        rebuild_site_participants(&site);
        if first_site {
            mm.uprobe_list.remove_if(probe_vaddr, &site);
        }
        consumer.forget_site(mm.id(), probe_vaddr, &site);
        site.state
            .store(UprobeSiteState::Dead as u8, Ordering::Release);
        return Err(e);
    }

    Ok(Some(UprobeHandle {
        mm: Arc::downgrade(mm),
        probe_vaddr,
        site: Some(site),
        consumer_id,
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
    let area = XolArea::new(page.virt_address(), page_paddr, owned_page);
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
pub(super) fn patch_byte_in_phys(
    page: &Arc<Page>,
    offset: usize,
    byte: u8,
) -> Result<(), SystemError> {
    let kva = unsafe { MMArch::phys_2_virt(page.phys_address()) }.ok_or(SystemError::EFAULT)?;
    unsafe {
        core::ptr::write_volatile((kva.data() + offset) as *mut u8, byte);
    }
    Ok(())
}

pub(super) fn uprobe_unregister_consumer_from_site(
    mm: &Arc<AddressSpace>,
    probe_vaddr: usize,
    site: &Arc<UprobeSite>,
    consumer_id: u64,
) {
    let mut inner = mm.write();
    uprobe_unregister_consumer_from_site_locked(mm, &mut inner, probe_vaddr, site, consumer_id);
}

fn uprobe_unregister_consumer_from_site_locked(
    mm: &Arc<AddressSpace>,
    inner: &mut RwSemWriteGuard<'_, InnerAddressSpace>,
    probe_vaddr: usize,
    site: &Arc<UprobeSite>,
    consumer_id: u64,
) {
    let Some(installed) = mm.uprobe_list.get(probe_vaddr) else {
        return;
    };
    if !Arc::ptr_eq(&installed, site) {
        return;
    }

    let (last_consumer, removed_consumer) = {
        let mut members = installed.members.lock();
        let Some(removed) = members.remove(&consumer_id) else {
            return;
        };
        (members.is_empty(), removed.consumer)
    };
    removed_consumer.forget_site(mm.id(), probe_vaddr, site);

    if !last_consumer {
        rebuild_site_participants(site);
        return;
    }

    let page_base_addr = probe_vaddr & !(MMArch::PAGE_SIZE - 1);
    let page_offset = probe_vaddr & (MMArch::PAGE_SIZE - 1);
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
    // Keep the old hit snapshot visible until the original byte is restored
    // and every CPU has passed the irq-off lookup window.
    if site_mapping_still_matches(inner, site)
        && restore_breakpoint_byte(
            mm,
            inner,
            page_base_addr,
            page_offset,
            site.old_instruction[0],
        )
    {
        mm.flush_tlb_range(
            VirtAddr::new(page_base_addr),
            VirtAddr::new(page_base_addr + MMArch::PAGE_SIZE),
            MMArch::PAGE_SHIFT as u8,
            false,
        );
    }
    mm.uprobe_list.remove_if(probe_vaddr, site);
    site.participants.store_deferred(Arc::new(Vec::new()));
    site.state
        .store(UprobeSiteState::Dead as u8, Ordering::Release);
    let mut pages = mm.uprobe_page_state.lock_irqsave();
    if let Some(state) = pages.get_mut(&page_base_addr) {
        debug_assert!(state.refcount != 0, "uprobe page refcount underflow");
        if state.refcount == 1 {
            pages.remove(&page_base_addr);
        } else {
            state.refcount -= 1;
        }
    } else {
        debug_assert!(false, "armed uprobe site without page state");
    }
}

fn rebuild_site_participants(site: &Arc<UprobeSite>) {
    let participants = site
        .members
        .lock()
        .values()
        .map(|member| member.target.clone())
        .collect();
    site.participants.store_deferred(Arc::new(participants));
}

/// 注销前按 Linux `register_for_each_vma()` 的方式在 mm 写锁下重验映射身份，
/// 防止 munmap 后同一虚址被无关文件复用时写坏新映射。VMA split 会创建新的
/// `LockedVMA` 对象，但保留相同的映射 lineage，因此不会丢失 retained segment。
pub(super) fn site_mapping_still_matches(inner: &InnerAddressSpace, site: &UprobeSite) -> bool {
    let Some(vma) = inner.mappings.contains(VirtAddr::new(site.probe_vaddr)) else {
        return false;
    };
    if vma.lineage_id() != site.mapping_lineage_id {
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
pub(super) fn restore_breakpoint_byte(
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

/// Remove reverse-index entries when an address space reaches final
/// destruction. Its PTEs and hit table disappear together, so no opcode
/// restoration is needed. Draining the per-mm table keeps the work
/// proportional to live sites instead of scanning every consumer's history.
pub(crate) fn uprobe_forget_address_space(mm: &AddressSpace) {
    let sites = mm.uprobe_list.take_all();
    for (vaddr, site) in sites {
        let members = core::mem::take(
            &mut *site
                .members
                .try_lock()
                .expect("final mm drop raced a live uprobe site writer"),
        );
        for member in members.into_values() {
            member.consumer.forget_site(mm.id(), vaddr, &site);
        }
        site.participants.store_deferred(Arc::new(Vec::new()));
        site.state
            .store(UprobeSiteState::Dead as u8, Ordering::Release);
    }
}
