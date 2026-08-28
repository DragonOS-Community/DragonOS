use super::hit_index::UprobeHitIndex;
use super::*;
use crate::mm::ucontext::MmuGather;

#[derive(Debug, Default)]
struct UprobeSiteControl {
    sites: BTreeMap<usize, Arc<UprobeSite>>,
    hit_index: UprobeHitIndex,
}

/// Per-address-space uprobe lookup table.
///
/// `control` is the only mutable source of truth. `hit_snapshot` is an
/// immutable RCU publication derived from it, so #BP lookup never serializes
/// CPUs on a shared lock. Writers publish the snapshot before exposing INT3
/// and withdraw it only after opcode restoration plus the TLB rendezvous.
#[derive(Debug)]
pub struct UprobeSiteTable {
    control: Mutex<UprobeSiteControl>,
    hit_snapshot: crate::rcu::RcuArcSlot<UprobeHitIndex>,
}

impl UprobeSiteTable {
    pub fn new() -> Self {
        Self {
            control: Mutex::new(UprobeSiteControl::default()),
            hit_snapshot: crate::rcu::RcuArcSlot::new(Arc::new(UprobeHitIndex::default())),
        }
    }

    /// Execute a bounded lookup while RCU pins the immutable map and site.
    /// The callback copies only the IRQ-safe values needed after it returns.
    pub fn with_hit<R>(&self, vaddr: usize, f: impl FnOnce(&UprobeSite) -> R) -> Option<R> {
        self.hit_snapshot
            .with_read(|sites| sites.get(vaddr).map(|site| f(site)))
    }

    pub(super) fn get(&self, vaddr: usize) -> Option<Arc<UprobeSite>> {
        self.control.lock().sites.get(&vaddr).cloned()
    }

    pub(super) fn range(&self, range: core::ops::Range<usize>) -> Vec<(usize, Arc<UprobeSite>)> {
        self.control
            .lock()
            .sites
            .range(range)
            .map(|(vaddr, site)| (*vaddr, site.clone()))
            .collect()
    }

    pub(super) fn insert(&self, vaddr: usize, site: Arc<UprobeSite>) -> Option<Arc<UprobeSite>> {
        let mut control = self.control.lock();
        let previous = control.sites.insert(vaddr, site.clone());
        control.hit_index.insert(vaddr, site);
        // Publish while the writer lock still orders this mutation. Publishing
        // after unlocking would let two writers store B then stale A, rolling
        // the #BP view back even though the control map already contains B.
        self.hit_snapshot
            .store_deferred(Arc::new(control.hit_index.clone()));
        previous
    }

    pub(super) fn remove_if(&self, vaddr: usize, expected: &Arc<UprobeSite>) -> bool {
        let mut control = self.control.lock();
        if !control
            .sites
            .get(&vaddr)
            .is_some_and(|site| Arc::ptr_eq(site, expected))
        {
            return false;
        }
        control.sites.remove(&vaddr);
        control.hit_index.remove(vaddr);
        self.hit_snapshot
            .store_deferred(Arc::new(control.hit_index.clone()));
        true
    }

    pub(super) fn take_all(&self) -> BTreeMap<usize, Arc<UprobeSite>> {
        core::mem::take(&mut self.control.lock().sites)
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
    participant_control: Mutex<UprobeParticipantControl>,
    /// Structurally shared hit-path snapshot. A writer prepends one node when
    /// a member is added and only rebuilds after stale removals become
    /// proportional to the live set. #BP pins one root and then only follows
    /// immutable links.
    participants: crate::rcu::RcuOptionArcSlot<UprobeParticipantNode>,
    pub(super) state: AtomicU8,
}

pub(super) struct UprobeSiteMember {
    pub(super) consumer: Arc<UprobeConsumer>,
    participant: Arc<UprobeSiteParticipant>,
}

#[derive(Default)]
struct UprobeParticipantControl {
    head: Option<Arc<UprobeParticipantNode>>,
    active: usize,
    stale: usize,
}

struct UprobeSiteParticipant {
    target: UprobeConsumerRuntimeSnapshot,
    active: AtomicBool,
}

pub(crate) struct UprobeParticipantNode {
    participant: Arc<UprobeSiteParticipant>,
    next: Option<Arc<UprobeParticipantNode>>,
}

impl UprobeParticipantNode {
    /// Visit the live participants in one pinned snapshot. Entries are never
    /// reactivated, so observing `false` cannot hide a later membership.
    pub(crate) fn for_each_active(&self, mut visit: impl FnMut(&UprobeConsumerRuntimeSnapshot)) {
        let mut node = Some(self);
        while let Some(current) = node {
            if current.participant.active.load(Ordering::Acquire) {
                visit(&current.participant.target);
            }
            node = current.next.as_deref();
        }
    }
}

impl Drop for UprobeParticipantNode {
    fn drop(&mut self) {
        // A directly nested Arc chain would otherwise recursively destroy one
        // stack frame per colocated consumer. `into_inner` elects at most one
        // owner to continue releasing a shared suffix, so even concurrent RCU
        // root retirement eventually destroys every uniquely owned suffix
        // iteratively.
        let mut next = self.next.take();
        while let Some(current) = next {
            let Some(mut current) = Arc::into_inner(current) else {
                break;
            };
            next = current.next.take();
        }
    }
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

    pub(crate) fn participant_snapshot(&self) -> Option<Arc<UprobeParticipantNode>> {
        self.participants.load()
    }

    fn add_member(
        &self,
        consumer_id: u64,
        consumer: Arc<UprobeConsumer>,
        target: UprobeConsumerRuntimeSnapshot,
    ) -> bool {
        let participant = Arc::new(UprobeSiteParticipant {
            target,
            active: AtomicBool::new(true),
        });
        let mut members = self.members.lock();
        if members.contains_key(&consumer_id) {
            return false;
        }
        let mut control = self.participant_control.lock();
        let head = Arc::new(UprobeParticipantNode {
            participant: participant.clone(),
            next: control.head.clone(),
        });
        members.insert(
            consumer_id,
            UprobeSiteMember {
                consumer,
                participant,
            },
        );
        control.head = Some(head);
        control.active += 1;
        true
    }

    /// Publish a root after its reverse index is ready. For the first site the
    /// root is completely prepared before the INT3 commit callback, so this
    /// operation only clones an Arc and swaps the RCU slot and F6 remains free
    /// of fallible preparation.
    fn publish_current_participants(&self) {
        let control = self.participant_control.lock();
        debug_assert!(control.head.is_some());
        self.participants.store_deferred(control.head.clone());
    }

    fn remove_member(&self, consumer_id: u64) -> Option<(bool, Arc<UprobeConsumer>)> {
        let mut members = self.members.lock();
        let removed = members.remove(&consumer_id)?;
        let last = members.is_empty();
        let mut control = self.participant_control.lock();
        let was_active = removed.participant.active.swap(false, Ordering::AcqRel);
        debug_assert!(was_active, "removed uprobe membership was already inactive");
        control.active = control
            .active
            .checked_sub(1)
            .expect("uprobe participant count underflow");
        control.stale += 1;

        if !last && control.stale >= control.active {
            let mut head = None;
            // Nodes are prepended, so the exact BTree iteration order is not
            // exposed as an ABI. Linux likewise prepends colocated consumers.
            for member in members.values() {
                head = Some(Arc::new(UprobeParticipantNode {
                    participant: member.participant.clone(),
                    next: head,
                }));
            }
            control.head = head;
            control.stale = 0;
            self.participants.store_deferred(control.head.clone());
        }
        Some((last, removed.consumer))
    }

    /// Withdraw the published root after the caller has made the breakpoint
    /// unreachable and completed the required TLB rendezvous. Already pinned
    /// roots remain valid through RCU and preserve the old-hit semantics.
    pub(super) fn withdraw_participants(&self) {
        let mut control = self.participant_control.lock();
        control.head = None;
        control.active = 0;
        control.stale = 0;
        self.participants.store_deferred(None);
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

impl core::fmt::Debug for XolPage {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("XolPage")
            .field("page_base", &self.page_base())
            .finish_non_exhaustive()
    }
}

/// Per-page state marker for installed breakpoints.
///
/// Keyed by the page base address (`probe_vaddr & !(PAGE_SIZE-1)`). Multiple uprobes
/// hitting the same page share one COW copy; `refcount` records the active breakpoint
/// count. Unregister restores bytes on the **currently mapped page** without swapping
/// pages (review R8), so only this counting marker is kept here (for the install path
/// to decide whether the page is already privatized).
pub(crate) struct UprobePageState {
    /// Number of active breakpoints.
    pub(super) refcount: usize,
}

impl core::fmt::Debug for UprobePageState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UprobePageState")
            .field("refcount", &self.refcount)
            .finish_non_exhaustive()
    }
}
// ──────────────────────── Registration handles ────────────────────────

/// Handle for a registered uprobe.
///
/// On drop, automatically rolls back any installation not yet handed off to
/// the mm hit table.
pub struct UprobeHandle {
    mm: Weak<AddressSpace>,
    probe_vaddr: usize,
    site: Option<Arc<UprobeSite>>,
    consumer_id: u64,
}

impl UprobeHandle {
    /// Transfer ownership to the consumer's weak site index; used for persistent mmap/fork installs.
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

pub(in crate::mm::ucontext) fn valid_probe_vma_flags(flags: VmFlags) -> bool {
    let invalid = VmFlags::VM_HUGETLB | VmFlags::VM_MAYSHARE | VmFlags::VM_WRITE;
    // Linux valid_vma(vma, true) deliberately keys registration on MAYEXEC,
    // not the current EXEC bit. Installing into an eligible NX mapping means
    // a later mprotect(PROT_EXEC) publishes an already-patched page instead of
    // racing a post-permission late install.
    flags.contains(VmFlags::VM_MAYEXEC) && !flags.intersects(invalid)
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

// ──────────────────────── Public API ────────────────────────

/// # Register an uprobe
///
/// Installs a 0xcc breakpoint at `probe_vaddr` in the target mm. Registration flow
/// (load order F6):
/// 1. Check whether `uprobe_list` already has an entry at the same address: if so,
///    reuse its old_instruction + insn_analysis (avoiding reading the 0xcc inside the
///    COW copy); otherwise read the original instruction and validate with `analyze_insn`;
/// 2. Allocate an XOL slot (filling `xol_slot_offset`);
/// 3. Call `build_xol_slot` with the real slot_vaddr to prefill the slot and validate
///    the RIP-relative displacement (overflow -> EINVAL fail-fast, never leaving a probe
///    that would panic on hit);
/// 4. Insert the `uprobe_list` entry;
/// 5. Install 0xcc (private COW, mirroring `do_wp_page`).
///
/// ## Parameters
/// - `mm`: the target address space.
/// - `probe_vaddr`: the user virtual address being probed (must lie in a mapped
///   executable VMA with the page present).
/// ## Returns
/// `Ok(UprobeHandle)` or an error code (`EINVAL`=invalid address/unsupported instruction,
/// `EFAULT`=page not mapped, `ENOMEM`=out of memory, `EACCES`=VMA not executable).
///
pub(super) fn uprobe_register(
    mm: &Arc<AddressSpace>,
    probe_vaddr: usize,
    consumer: &Arc<UprobeConsumer>,
    expected_mapping: &ExpectedProbeMapping,
) -> Result<Option<UprobeHandle>, SystemError> {
    let result = loop {
        let result = {
            let mut inner = mm.write();
            uprobe_register_locked(mm, &mut inner, probe_vaddr, consumer, expected_mapping)
        };
        match result {
            Err(LockedRegisterError::PageContended(page)) => {
                // Wait for the exact conflicting Page only after releasing
                // mm.write, then retry with full mapping/epoch revalidation.
                // Taking a write guard handles both reader and writer
                // contention without speculative map/unmap retry storms.
                drop(page.write());
            }
            other => break other,
        }
    };
    let handle = match result.map_err(LockedRegisterError::into_system_error) {
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

pub(super) enum LockedRegisterError {
    System(SystemError),
    PageContended(Arc<Page>),
}

impl LockedRegisterError {
    fn into_system_error(self) -> SystemError {
        match self {
            Self::System(error) => error,
            Self::PageContended(_) => SystemError::EAGAIN_OR_EWOULDBLOCK,
        }
    }
}

impl From<SystemError> for LockedRegisterError {
    fn from(error: SystemError) -> Self {
        Self::System(error)
    }
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
) -> Result<Option<UprobeHandle>, LockedRegisterError> {
    let install = consumer.begin_install(mm).ok_or(SystemError::ENOENT)?;
    let consumer_id = consumer.id;

    // ── Step 1: locate VMA + read original instruction + analyze ──
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

    // ── P1: when re-registering the same probe_vaddr, reuse existing instruction info (avoid reading 0xcc) ──
    // A second consumer registering the same address finds the PTE already pointing at a COW
    // copy containing 0xcc, which read_user_insn_bytes would mistake for the original instruction.
    // Consult uprobe_list first: if an entry exists at the same address, reuse its
    // old_instruction + insn_analysis (identical for all same-address instances), skipping the read.
    let existing_site = mm.uprobe_list.get(probe_vaddr);

    if existing_site
        .as_ref()
        .is_some_and(|site| site.members.lock().contains_key(&consumer_id))
    {
        return Ok(None);
    }

    let (old_instruction, analysis) = if let Some(site) = existing_site.as_ref() {
        if !Arc::ptr_eq(&site.definition, &consumer.definition) {
            return Err(SystemError::EINVAL.into());
        }
        (site.old_instruction, site.insn_analysis)
    } else {
        consumer.definition.instruction()
    };

    // ── Step 2: ensure the XOL area exists + allocate a slot ──
    let (xol_lease, fresh_xol_page) = if let Some(site) = existing_site.as_ref() {
        (site.xol_lease.clone(), None)
    } else {
        let reachable = ::uprobe::xol_slot_vaddr_range(&analysis, probe_vaddr);
        ensure_xol_and_alloc_slot(mm, inner, &reachable)?
    };
    let xol_slot_offset = xol_lease.offset();

    // ── P2: prefill the XOL slot at registration + validate the RIP-relative displacement (fail-fast) ──
    // slot_vaddr = xol_page_base + slot_offset is now known; call build_xol_slot with the real address
    // immediately: displacement overflow -> EINVAL (registration fails, no hit-time-panicking probe is
    // introduced); on success the slot content is written to the physical page, ready for rip on #BP hit.
    if existing_site.is_none() {
        let (slot_vaddr, page_paddr) = { (xol_lease.slot_vaddr(), xol_lease.page_paddr()) };

        let fill_result = (|| {
            let mut slot_buf = [0u8; UPROBE_INSN_COPY_SIZE];
            build_xol_slot(
                &analysis,
                probe_vaddr,
                slot_vaddr.data(),
                &old_instruction,
                &mut slot_buf,
            )
            .map_err(|e| {
                log::warn!(
                    "uprobe_register: build_xol_slot failed at {:#x} (slot {:#x}): {:?}",
                    probe_vaddr,
                    slot_vaddr.data(),
                    e
                );
                SystemError::EINVAL
            })?;

            // Write the XOL slot into the physical page (mirroring batch3's fill_xol_slot / patch_byte_in_phys).
            let kva = unsafe { MMArch::phys_2_virt(page_paddr) }.ok_or(SystemError::EFAULT)?;
            unsafe {
                let dst = (kva.data() + xol_slot_offset) as *mut u8;
                core::ptr::copy_nonoverlapping(slot_buf.as_ptr(), dst, UPROBE_INSN_COPY_SIZE);
            }
            Ok::<(), SystemError>(())
        })();
        if let Err(error) = fill_result {
            if let Some(page) = fresh_xol_page.as_ref() {
                drop(xol_lease);
                discard_unpublished_xol_page(mm, inner, page);
            }
            return Err(error.into());
        }
    }

    // ── Step 3: create the uprobe entity ──
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
            participant_control: Mutex::new(UprobeParticipantControl::default()),
            participants: crate::rcu::RcuOptionArcSlot::new_none(),
            state: AtomicU8::new(UprobeSiteState::Prepared as u8),
        })
    });

    let inserted = site.add_member(consumer_id, consumer.clone(), install.hit_target());
    debug_assert!(inserted, "duplicate uprobe membership passed precheck");

    let install_result = if first_site {
        // Prepare and validate the exact COW bytes before publishing any hit
        // metadata. The callback is invoked only after every fallible step and
        // immediately before the infallible INT3/PTE commit, preserving F6
        // without exposing an XOL lease from a failed installation.
        let publish_site = || {
            let previous = mm.uprobe_list.insert(probe_vaddr, site.clone());
            debug_assert!(previous.is_none());
            consumer.remember_site(mm, probe_vaddr, &site);
            site.publish_current_participants();
            site.state
                .store(UprobeSiteState::Armed as u8, Ordering::Release);
        };
        install_breakpoint_page(
            mm,
            inner,
            &vma,
            probe_vaddr,
            page_base_addr,
            page_offset,
            &old_instruction,
            analysis.insn_len,
            publish_site,
        )
    } else {
        // The breakpoint and hit-table entry already exist. Publish only the
        // new consumer membership and its reverse index.
        consumer.remember_site(mm, probe_vaddr, &site);
        site.publish_current_participants();
        Ok(())
    };
    if let Err(e) = install_result {
        let removed = site.remove_member(consumer_id);
        debug_assert!(removed.is_some());
        if first_site {
            mm.uprobe_list.remove_if(probe_vaddr, &site);
        }
        consumer.forget_site(mm.id(), probe_vaddr, &site);
        site.state
            .store(UprobeSiteState::Dead as u8, Ordering::Release);
        if let Some(page) = fresh_xol_page {
            // No breakpoint was published, so this newly grown XOL page has
            // no external user. Drop every slot owner before removing the
            // exact kernel-owned VMA; failed registrations must not grow the
            // per-mm pool high-water mark.
            drop(site);
            drop(xol_lease);
            discard_unpublished_xol_page(mm, inner, &page);
        }
        return Err(e);
    }

    if let Some(page) = fresh_xol_page {
        mm.xol_pool.add_page(page);
    }

    Ok(Some(UprobeHandle {
        mm: Arc::downgrade(mm),
        probe_vaddr,
        site: Some(site),
        consumer_id,
    }))
}

// ──────────────────────── Internal implementation ────────────────────────

/// Copy instruction bytes after the caller has locked every backing Page.
fn copy_locked_instruction(
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

/// Allocate an XOL slot, growing the per-mm pool by one page when necessary.
fn ensure_xol_and_alloc_slot(
    mm: &Arc<AddressSpace>,
    inner: &mut RwSemWriteGuard<'_, InnerAddressSpace>,
    reachable: &core::ops::RangeInclusive<usize>,
) -> Result<(Arc<XolSlotLease>, Option<Arc<XolPage>>), SystemError> {
    if let Some(lease) = mm.xol_pool.alloc_slot_in(reachable) {
        return Ok((lease, None));
    }

    // Reserve the collection entry before committing a new VMA. All callers
    // own mm.write, so no competing registration can consume this capacity.
    mm.xol_pool.reserve_page()?;

    // Select a whole-page hole whose base is itself reachable. Slot zero of
    // the fresh page is then guaranteed to satisfy the exact disp32 interval,
    // and MAP_FIXED_NOREPLACE below cannot silently fall back elsewhere.
    let page_mask = MMArch::PAGE_SIZE - 1;
    let first = (*reachable.start())
        .max(inner.mmap_min.data())
        .checked_add(page_mask)
        .map(|addr| addr & !page_mask)
        .ok_or(SystemError::ENOMEM)?;
    let last = (*reachable.end()).min(
        MMArch::USER_END_VADDR
            .data()
            .checked_sub(MMArch::PAGE_SIZE)
            .ok_or(SystemError::ENOMEM)?,
    ) & !page_mask;
    if first > last {
        return Err(SystemError::ENOMEM);
    }
    let bounded_size = last
        .checked_sub(first)
        .and_then(|size| size.checked_add(MMArch::PAGE_SIZE))
        .ok_or(SystemError::ENOMEM)?;
    let bounds = VirtRegion::new(VirtAddr::new(first), bounded_size);
    let region = inner
        .mappings
        .find_free_bounded(bounds, MMArch::PAGE_SIZE)
        .ok_or(SystemError::ENOMEM)?;

    // Slow path: create another anonymous read/execute page. map_anonymous may
    // allocate and sleep; no XOL pool or slot lock is held here.
    let prot = ProtFlags::PROT_READ | ProtFlags::PROT_EXEC;
    let map_flags = MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS | MapFlags::MAP_FIXED_NOREPLACE;
    let page = inner.map_anonymous(
        region.start(),
        MMArch::PAGE_SIZE,
        prot,
        map_flags,
        true, // round_to_min
        true, // allocate_at_once (allocate zero pages immediately)
    )?;

    // XOL is kernel-owned execution state, not an ordinary anonymous mapping.
    // It must never be copied into a child mm or expanded/remapped as user data.
    // VM_IO also makes MADV_DOFORK reject attempts to clear VM_DONTCOPY, matching
    // the invariant that every mm owns independently managed XOL pages.
    let xol_vma = inner
        .mappings
        .contains(page.virt_address())
        .expect("fresh XOL mapping disappeared under mm.write");
    {
        let mut guard = xol_vma.lock();
        let special =
            VmFlags::VM_DONTCOPY | VmFlags::VM_IO | VmFlags::VM_DONTEXPAND | VmFlags::VM_DONTDUMP;
        let flags = *guard.vm_flags() | special;
        guard.set_vm_flags(flags);
    }

    // Get the XOL page physical address (for batch3's irq-off path to write slot contents)
    let page_paddr = inner
        .user_mapper
        .utable
        .translate(page.virt_address())
        .map(|(pa, _)| pa)
        .expect("allocate_at_once XOL mapping has no PTE");

    let owned_page = {
        let mut pm = page_manager_lock();
        pm.get(&page_paddr)
            .expect("fresh XOL page is absent from the page manager")
    };
    let xol_page = XolPage::new(page.virt_address(), page_paddr, owned_page);
    let lease = xol_page
        .alloc_slot_in(reachable)
        .map(Arc::new)
        .expect("fresh reachable XOL page has no reachable slot");
    Ok((lease, Some(xol_page)))
}

/// Remove an XOL VMA which was created for an installation that never
/// published a breakpoint. The VMA is exact, anonymous, and has no consumer,
/// so the generic fallible munmap transaction would only add rollback states.
fn discard_unpublished_xol_page(
    mm: &Arc<AddressSpace>,
    inner: &mut RwSemWriteGuard<'_, InnerAddressSpace>,
    page: &Arc<XolPage>,
) {
    let region = VirtRegion::new(page.page_base(), MMArch::PAGE_SIZE);
    let vma = inner
        .mappings
        .remove_vma(&region)
        .expect("unpublished XOL VMA disappeared under mm.write");
    let mut tlb = MmuGather::gather(mm);
    vma.unmap(&mut inner.user_mapper.utable, &mut tlb);
    tlb.finish();
}

/// Install the 0xcc breakpoint page (mirroring do_wp_page private-file COW).
///
/// If the page already has a breakpoint (another uprobe on the same physical page),
/// only patch the extra 0xcc bytes + refcount++; otherwise COW → patch → single
/// set_entry → rmap → flush_tlb_range.
#[allow(clippy::too_many_arguments)]
fn install_breakpoint_page<F>(
    mm: &Arc<AddressSpace>,
    inner: &mut RwSemWriteGuard<'_, InnerAddressSpace>,
    vma: &Arc<LockedVMA>,
    probe_vaddr: usize,
    page_base_addr: usize,
    page_offset: usize,
    old_instruction: &[u8; UPROBE_INSN_COPY_SIZE],
    insn_len: usize,
    publish_site: F,
) -> Result<(), LockedRegisterError>
where
    F: FnOnce(),
{
    let address = VirtAddr::new(page_base_addr);
    let end = VirtAddr::new(page_base_addr + MMArch::PAGE_SIZE);

    // ── check whether the page already has a breakpoint (multiple uprobes on one page) ──
    let already_cowed = {
        let pb = mm.uprobe_page_state.lock_irqsave();
        pb.contains_key(&page_base_addr)
    };

    if already_cowed {
        // Validate and patch the current private page under one try-only Page
        // lock. Blocking here under mm.write would invert writeback's Page ->
        // mm ordering.
        {
            let mapper = &mut inner.user_mapper.utable;
            let (paddr, _) = mapper.translate(address).ok_or(SystemError::EFAULT)?;
            let page = page_manager_lock().get(&paddr).ok_or(SystemError::EFAULT)?;
            let instruction_end = probe_vaddr
                .checked_add(insn_len)
                .ok_or(SystemError::EFAULT)?;
            let second_page = if instruction_end > page_base_addr + MMArch::PAGE_SIZE {
                let second_addr = VirtAddr::new(page_base_addr + MMArch::PAGE_SIZE);
                let (second_paddr, _) = mapper.translate(second_addr).ok_or(SystemError::EFAULT)?;
                Some((
                    second_paddr,
                    page_manager_lock()
                        .get(&second_paddr)
                        .ok_or(SystemError::EFAULT)?,
                ))
            } else {
                None
            };
            let (_first_guard, _second_guard) = match second_page.as_ref() {
                Some((second_paddr, second)) if *second_paddr != paddr => {
                    if paddr.data() <= second_paddr.data() {
                        let first = page
                            .try_write()
                            .ok_or_else(|| LockedRegisterError::PageContended(page.clone()))?;
                        let second = second
                            .try_write()
                            .ok_or_else(|| LockedRegisterError::PageContended(second.clone()))?;
                        (first, Some(second))
                    } else {
                        let second_guard = second
                            .try_write()
                            .ok_or_else(|| LockedRegisterError::PageContended(second.clone()))?;
                        let first = page
                            .try_write()
                            .ok_or_else(|| LockedRegisterError::PageContended(page.clone()))?;
                        (first, Some(second_guard))
                    }
                }
                _ => (
                    page.try_write()
                        .ok_or_else(|| LockedRegisterError::PageContended(page.clone()))?,
                    None,
                ),
            };
            let mut mapped = [0u8; UPROBE_INSN_COPY_SIZE];
            copy_locked_instruction(mapper, probe_vaddr, &mut mapped[..insn_len])?;
            if mapped[..insn_len] != old_instruction[..insn_len] {
                return Err(SystemError::EINVAL.into());
            }
            let kva =
                unsafe { MMArch::phys_2_virt(page.phys_address()) }.ok_or(SystemError::EFAULT)?;

            publish_site();
            // All fallible preparation is complete. Publish page-state before
            // the byte so teardown can account for every visible INT3.
            mm.uprobe_page_state
                .lock_irqsave()
                .get_mut(&page_base_addr)
                .expect("existing uprobe COW page lost its state")
                .refcount += 1;
            let pt_edit = mm.page_table_edit();
            unsafe {
                core::ptr::write_volatile((kva.data() + page_offset) as *mut u8, 0xcc);
            }
            drop(pt_edit);
            // Keep both source-page guards until stale executable translations
            // have passed the new INT3 publication point.
            mm.flush_tlb_range(address, end, MMArch::PAGE_SHIFT as u8, false);
        }
        return Ok(());
    }

    // ── New COW breakpoint page ──

    let mapper = &mut inner.user_mapper.utable;

    // translate to get the old paddr + flags
    let (old_paddr, entry_flags) = mapper.translate(address).ok_or(SystemError::EFAULT)?;

    // Fetch the old page (must be tracked by page_manager -- a File or Normal page)
    let old_page = {
        let mut pm = page_manager_lock();
        pm.get(&old_paddr).ok_or(SystemError::EFAULT)?
    };

    // Allocate an unpublished destination before taking source Page locks, so
    // PageManager -> Page remains the only blocking lock order.
    let new_page = {
        let mut pm = page_manager_lock();
        pm.create_one_page(PageType::Normal, PageFlags::empty(), mapper.allocator_mut())?
    };
    let install_result = (|| -> Result<(), LockedRegisterError> {
        let instruction_end = probe_vaddr
            .checked_add(insn_len)
            .ok_or(SystemError::EFAULT)?;
        let second_page = if instruction_end > page_base_addr + MMArch::PAGE_SIZE {
            let second_addr = VirtAddr::new(page_base_addr + MMArch::PAGE_SIZE);
            let (second_paddr, _) = mapper.translate(second_addr).ok_or(SystemError::EFAULT)?;
            Some((
                second_paddr,
                page_manager_lock()
                    .get(&second_paddr)
                    .ok_or(SystemError::EFAULT)?,
            ))
        } else {
            None
        };

        let mut new_guard = new_page.write();
        // x86 instructions span at most two pages. Try-lock both in physical
        // order; a concurrent writer makes registration retry instead of
        // creating an mm -> Page wait edge.
        let (mut first_guard, mut second_guard) = match second_page.as_ref() {
            Some((second_paddr, second)) if *second_paddr != old_paddr => {
                if old_paddr.data() <= second_paddr.data() {
                    let first = old_page
                        .try_write()
                        .ok_or_else(|| LockedRegisterError::PageContended(old_page.clone()))?;
                    let second = second
                        .try_write()
                        .ok_or_else(|| LockedRegisterError::PageContended(second.clone()))?;
                    (Some(first), Some(second))
                } else {
                    let second = second
                        .try_write()
                        .ok_or_else(|| LockedRegisterError::PageContended(second.clone()))?;
                    let first = old_page
                        .try_write()
                        .ok_or_else(|| LockedRegisterError::PageContended(old_page.clone()))?;
                    (Some(first), Some(second))
                }
            }
            _ => (
                Some(
                    old_page
                        .try_write()
                        .ok_or_else(|| LockedRegisterError::PageContended(old_page.clone()))?,
                ),
                None,
            ),
        };

        let old_guard = first_guard.as_mut().expect("first source page guard");
        unsafe { new_guard.copy_from_slice(old_guard.as_slice()) };
        new_guard.set_flags(*old_guard.flags());
        new_guard.clear_mapping_unevictable_source_for_cow();

        let mut mapped = [0u8; UPROBE_INSN_COPY_SIZE];
        // Validate the bytes that will actually become executable. A writable
        // MAP_SHARED alias can modify the source through hardware without
        // taking the Page lock, so re-reading only the source after the copy
        // could accept a mixed-version destination. The first-page bytes come
        // from the unpublished COW candidate; only a cross-page tail remains
        // backed by (and is read from) the locked source mapping.
        let first_len = insn_len.min(MMArch::PAGE_SIZE - page_offset);
        unsafe {
            mapped[..first_len]
                .copy_from_slice(&new_guard.as_slice()[page_offset..page_offset + first_len]);
        }
        if first_len < insn_len {
            copy_locked_instruction(
                mapper,
                probe_vaddr + first_len,
                &mut mapped[first_len..insn_len],
            )?;
        }
        if mapped[..insn_len] != old_instruction[..insn_len] {
            return Err(SystemError::EINVAL.into());
        }
        patch_byte_in_phys(&new_page, page_offset, 0xcc)?;

        // Complete all bookkeeping which may allocate before publishing the
        // hit metadata. mm.write keeps the original PTE stable throughout.
        let vm_locked = vma.lock().vm_flags().contains(VmFlags::VM_LOCKED);
        new_guard.insert_vma(vma.clone(), vm_locked);
        mm.uprobe_page_state
            .lock_irqsave()
            .insert(page_base_addr, UprobePageState { refcount: 1 });

        publish_site();
        // No fallible operation follows site publication. Revalidate the PTE
        // as an invariant under mm.write, then atomically expose the patched
        // private page.
        let pt_edit = mm.page_table_edit();
        let (current_paddr, _) = mapper
            .translate(address)
            .expect("prepared uprobe source PTE disappeared under mm.write");
        assert_eq!(current_paddr, old_paddr);
        let table = mapper
            .get_table(address, 0)
            .expect("prepared uprobe page table disappeared under mm.write");
        let i = table
            .index_of(address)
            .expect("prepared uprobe address left its page table");
        unsafe {
            table.set_entry(i, PageEntry::new(new_page.phys_address(), entry_flags));
        }
        drop(pt_edit);

        // The old source Page remains locked until every CPU has discarded a
        // possibly stale executable translation. Only then may writeback
        // mutate it or stop accounting this VMA in its reverse map.
        mm.flush_tlb_range(address, end, MMArch::PAGE_SHIFT as u8, false);
        old_guard.remove_vma(vma.as_ref());
        let should_reclaim_old = old_guard.flags().contains(PageFlags::PG_UNEVICTABLE)
            && !old_guard.has_unevictable_source();
        if should_reclaim_old {
            old_guard.remove_flags(PageFlags::PG_UNEVICTABLE);
        }
        let old_was_lru = old_guard.flags().contains(PageFlags::PG_LRU);
        drop(second_guard.take());
        drop(first_guard.take());
        drop(new_guard);
        if should_reclaim_old && old_was_lru {
            page_reclaimer_lock().insert_page(old_paddr, &old_page);
        }
        Ok(())
    })();

    if let Err(error) = install_result {
        // No PTE points at the destination on failure; remove PageManager's
        // owning reference after all page guards have unwound.
        page_manager_lock().remove_page_if_same(&new_page);
        return Err(error);
    }
    Ok(())
}

/// Write one byte at the given offset of a physical page (via the kernel direct map).
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

    let Some((last_consumer, removed_consumer)) = installed.remove_member(consumer_id) else {
        return;
    };
    removed_consumer.forget_site(mm.id(), probe_vaddr, site);

    if !last_consumer {
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
    site.withdraw_participants();
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

/// Before unregistering, revalidate the mapping identity under the mm write lock the way
/// Linux `register_for_each_vma()` does, to avoid corrupting a new mapping when the same
/// virtual address is reused by an unrelated file after munmap. VMA split creates new
/// `LockedVMA` objects but preserves the same mapping lineage, so the retained segment
/// is not lost.
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

/// Restore the original breakpoint byte on the **currently mapped page** (review R8).
///
/// Use `translate` to get the current paddr (the COW copy from breakpoint installation,
/// or a page from a second COW after a program write fault). As with Linux
/// `verify_opcode()`, restore only while the current byte is still the INT3 this
/// facility installed; a new instruction written externally (e.g. by a debugger) must
/// never be overwritten by close. The page mapping is not swapped, so program writes to
/// other bytes remain. No PTE change → no TLB flush (TLB caches translations, not
/// contents; cross-modifying-code serialization is provided by the synchronous rendezvous).
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
                let opcode = (kva.data() + page_offset) as *mut u8;
                if core::ptr::read_volatile(opcode) != 0xcc {
                    return false;
                }
                core::ptr::write_volatile(opcode, orig_first_byte);
            }
            return true;
        }
    }
    // The page was munmapped (translate failed): nothing to restore.
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
        site.withdraw_participants();
        site.state
            .store(UprobeSiteState::Dead as u8, Ordering::Release);
    }
}
