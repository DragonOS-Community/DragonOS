use super::*;

// ──────────────────────────── XOL area ────────────────────────────

/// Width of each slot (= `UPROBE_INSN_COPY_SIZE` = 16 bytes).
const XOL_SLOT_SIZE: usize = UPROBE_INSN_COPY_SIZE;

/// Number of slots per page (4096 / 16 = 256).
const XOL_SLOTS_PER_PAGE: usize = MMArch::PAGE_SIZE / XOL_SLOT_SIZE;

/// Number of u64 words needed for the slot bitmap (256 bits -> 4 words).
const XOL_BITMAP_WORDS: usize = XOL_SLOTS_PER_PAGE.div_ceil(64);

fn take_reachable_slot(
    bitmap: &mut [u64; XOL_BITMAP_WORDS],
    page_base: usize,
    reachable: &core::ops::RangeInclusive<usize>,
) -> Option<usize> {
    for (word_idx, word) in bitmap.iter_mut().enumerate() {
        let mut free = !*word;
        while free != 0 {
            let bit = free.trailing_zeros() as usize;
            let slot = word_idx * 64 + bit;
            if slot >= XOL_SLOTS_PER_PAGE {
                break;
            }
            let offset = slot * XOL_SLOT_SIZE;
            let slot_vaddr = page_base.checked_add(offset)?;
            if !reachable.contains(&slot_vaddr) {
                free &= free - 1;
                continue;
            }
            *word |= 1u64 << bit;
            return Some(offset);
        }
    }
    None
}

/// One page in a per-mm XOL (eXecute Out of Line) pool.
///
/// The page is mapped read/execute in userspace and divided into 16-byte
/// slots. Pages are added to [`XolPool`] only from the registration path;
/// the exception path never grows the pool or allocates memory.
pub struct XolPage {
    /// Base address of the XOL page in user space.
    page_base: VirtAddr,
    /// Physical address of the XOL page (for batch3 to write slot contents directly via
    /// `phys_2_virt` on the interrupts-disabled path, without mapper / RwSem).
    page_paddr: PhysAddr,
    /// Keeps the XOL physical page alive for the whole lease lifetime; never keep only a raw paddr.
    _page: Arc<Page>,
    /// Area generation: prevents an old lease from freeing the same-numbered slot in a new area.
    generation: u64,
    /// Slot allocation bitmap (bit=1 means occupied).
    slot_bitmap: SpinLock<[u64; XOL_BITMAP_WORDS]>,
}

impl XolPage {
    pub(super) fn new(page_base: VirtAddr, page_paddr: PhysAddr, page: Arc<Page>) -> Arc<Self> {
        Arc::new(Self {
            page_base,
            page_paddr,
            _page: page,
            generation: NEXT_XOL_GENERATION.fetch_add(1, Ordering::Relaxed),
            slot_bitmap: SpinLock::new([0u64; XOL_BITMAP_WORDS]),
        })
    }

    pub(super) fn alloc_slot_in(
        self: &Arc<Self>,
        reachable: &core::ops::RangeInclusive<usize>,
    ) -> Option<XolSlotLease> {
        let mut bitmap = self.slot_bitmap.lock_irqsave();
        let offset = take_reachable_slot(&mut bitmap, self.page_base.data(), reachable)?;
        Some(XolSlotLease {
            page: self.clone(),
            offset,
            generation: self.generation,
        })
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

    /// Compute the user virtual address for a slot (used by batch3).
    pub fn slot_vaddr(&self, offset: usize) -> VirtAddr {
        VirtAddr::new(self.page_base.data() + offset)
    }

    /// Base address of the XOL page (used by batch3 to compute slot addresses).
    pub fn page_base(&self) -> VirtAddr {
        self.page_base
    }

    /// XOL page physical address (batch3 writes slot contents via `phys_2_virt` when interrupts are off).
    pub fn page_paddr(&self) -> PhysAddr {
        self.page_paddr
    }
}

/// Exclusive ownership lease for one XOL slot. The hit path should store the `Arc<XolSlotLease>`
/// in `ActiveXol`, so deregistration revokes only future hits, never reusing a still-running slot.
pub struct XolSlotLease {
    page: Arc<XolPage>,
    offset: usize,
    generation: u64,
}

impl XolSlotLease {
    pub fn offset(&self) -> usize {
        self.offset
    }

    pub fn slot_vaddr(&self) -> VirtAddr {
        self.page.slot_vaddr(self.offset)
    }

    pub fn page_paddr(&self) -> PhysAddr {
        self.page.page_paddr()
    }

    pub fn page(&self) -> &Arc<XolPage> {
        &self.page
    }
}

impl Drop for XolSlotLease {
    fn drop(&mut self) {
        self.page.free_slot(self.offset, self.generation);
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

/// Growable collection of immutable XOL pages owned by one address space.
///
/// DragonOS deliberately assigns one pre-relocated slot to each installed
/// site so the #BP path remains allocation-free. Consequently a fixed
/// one-page area would incorrectly cap an mm at 256 registered addresses.
/// The pool grows one page at a time on the registration cold path instead.
pub struct XolPool {
    pages: Mutex<Vec<Arc<XolPage>>>,
}

impl core::fmt::Debug for XolPool {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // AddressSpace formatting may happen on diagnostic paths. Do not take
        // the sleeping pool mutex merely to report an advisory page count.
        f.debug_struct("XolPool").finish_non_exhaustive()
    }
}

impl XolPool {
    pub fn new() -> Self {
        Self {
            pages: Mutex::new(Vec::new()),
        }
    }

    pub(super) fn alloc_slot_in(
        &self,
        reachable: &core::ops::RangeInclusive<usize>,
    ) -> Option<Arc<XolSlotLease>> {
        let pages = self.pages.lock();
        let first = pages.partition_point(|page| {
            page.page_base.data() + MMArch::PAGE_SIZE - XOL_SLOT_SIZE < *reachable.start()
        });
        let end = pages.partition_point(|page| page.page_base.data() <= *reachable.end());
        pages[first..end]
            .iter()
            // The newest page is normally the only partially filled one, so
            // monotonic registration remains O(1) instead of rescanning every
            // older compatible page for each new site. Pages outside the exact
            // disp32 interval are excluded by the two binary searches above.
            .rev()
            .find_map(|page| page.alloc_slot_in(reachable).map(Arc::new))
    }

    /// Reserve the collection entry before mapping a new page. Registration
    /// is serialized by mm.write, so this capacity cannot be consumed by a
    /// competing grow operation before [`Self::add_page`] is called.
    pub(super) fn reserve_page(&self) -> Result<(), SystemError> {
        self.pages
            .lock()
            .try_reserve(1)
            .map_err(|_| SystemError::ENOMEM)
    }

    pub(super) fn add_page(&self, page: Arc<XolPage>) {
        let mut pages = self.pages.lock();
        debug_assert!(pages.len() < pages.capacity());
        let index = pages
            .binary_search_by_key(&page.page_base.data(), |entry| entry.page_base.data())
            .expect_err("duplicate XOL page base");
        pages.insert(index, page);
    }

    pub(in crate::mm::ucontext) fn overlaps(&self, region: VirtRegion) -> bool {
        let pages = self.pages.lock();
        let first = pages.partition_point(|page| {
            page.page_base.data() + MMArch::PAGE_SIZE <= region.start().data()
        });
        pages.get(first).is_some_and(|page| {
            VirtRegion::new(page.page_base(), MMArch::PAGE_SIZE).collide(&region)
        })
    }
}

impl Default for XolPool {
    fn default() -> Self {
        Self::new()
    }
}

static NEXT_XOL_GENERATION: AtomicU64 = AtomicU64::new(1);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unreachable_slots_are_not_consumed() {
        let mut bitmap = [0u64; XOL_BITMAP_WORDS];
        let original = bitmap;
        assert_eq!(
            take_reachable_slot(&mut bitmap, 0x1000, &(0x8000..=0x8fff)),
            None
        );
        assert_eq!(bitmap, original);
    }

    #[test]
    fn only_a_reachable_free_slot_is_consumed() {
        let mut bitmap = [0u64; XOL_BITMAP_WORDS];
        let page_base = 0x4000;
        let wanted = page_base + 7 * XOL_SLOT_SIZE;
        assert_eq!(
            take_reachable_slot(&mut bitmap, page_base, &(wanted..=wanted)),
            Some(7 * XOL_SLOT_SIZE)
        );
        assert_eq!(bitmap[0], 1 << 7);
    }
}
