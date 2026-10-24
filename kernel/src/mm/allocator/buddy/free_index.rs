//! Sparse physical-head lookup and order bitmaps. Every address refers to
//! allocator metadata; free-page payload is never used as an index node.
use super::{BUDDY_ORDER_COUNT, MIN_ORDER};
use crate::mm::{MemoryManagementArch, PhysAddr, PhysMemoryArea};
use core::{marker::PhantomData, mem};

const WORD_BYTES: usize = mem::size_of::<usize>();
const FANOUT: usize = 4096 / WORD_BYTES;
const SPAN_WORDS: usize = 64;
const MAX_LEVELS: usize = usize::BITS as usize / 6 + 1;
const _: () = assert!(3 + BUDDY_ORDER_COUNT * 3 <= SPAN_WORDS);

#[derive(Clone, Copy)]
struct Bitmap {
    first: usize,
    bits: usize,
    offset: usize,
}

#[derive(Clone, Copy)]
struct Span {
    first: usize,
    end: usize,
    slots: usize,
    descriptor: usize,
}

#[derive(Debug)]
struct WordStore<A> {
    root: PhysAddr,
    /// Zero means the root itself is a data page.
    height: usize,
    words: usize,
    marker: PhantomData<A>,
    #[cfg(feature = "mm_selftest")]
    owner: Option<FixturePages>,
}

impl<A: MemoryManagementArch> WordStore<A> {
    unsafe fn new(words: usize, allocate: &mut impl FnMut() -> Option<PhysAddr>) -> Option<Self> {
        if A::PAGE_SIZE != 4096 || WORD_BYTES != 8 || words == 0 {
            return None;
        }
        let pages = words.checked_add(FANOUT - 1)? / FANOUT;
        let mut height = 0;
        let mut capacity = 1usize;
        while capacity < pages {
            capacity = capacity.checked_mul(FANOUT)?;
            height += 1;
        }
        unsafe fn build<A: MemoryManagementArch>(
            height: usize,
            pages: usize,
            allocate: &mut impl FnMut() -> Option<PhysAddr>,
        ) -> Option<PhysAddr> {
            let base = allocate()?;
            if base.is_null() || !base.check_aligned(A::PAGE_SIZE) {
                return None;
            }
            core::ptr::write_bytes(A::phys_2_virt(base)?.as_ptr::<u8>(), 0, A::PAGE_SIZE);
            if height != 0 {
                let child_capacity = FANOUT.checked_pow((height - 1).try_into().ok()?)?;
                let children = pages.checked_add(child_capacity - 1)? / child_capacity;
                for child in 0..children {
                    let count = (pages - child * child_capacity).min(child_capacity);
                    let address = build::<A>(height - 1, count, allocate)?;
                    A::write(A::phys_2_virt(base + child * WORD_BYTES)?, address);
                }
            }
            Some(base)
        }
        Some(Self {
            root: build::<A>(height, pages, allocate)?,
            height,
            words,
            marker: PhantomData,
            #[cfg(feature = "mm_selftest")]
            owner: None,
        })
    }

    fn physical(&self, word: usize) -> PhysAddr {
        assert!(word < self.words, "free index word outside storage");
        let page = word / FANOUT;
        let mut address = self.root;
        for level in (0..self.height).rev() {
            let child = (page / FANOUT.pow(level as u32)) % FANOUT;
            address = unsafe { A::read(A::phys_2_virt(address + child * WORD_BYTES).unwrap()) };
            assert!(!address.is_null(), "missing free index storage page");
        }
        address + word % FANOUT * WORD_BYTES
    }

    fn read(&self, word: usize) -> usize {
        unsafe { A::read(A::phys_2_virt(self.physical(word)).unwrap()) }
    }

    fn write(&self, word: usize, value: usize) {
        unsafe { A::write(A::phys_2_virt(self.physical(word)).unwrap(), value) }
    }

    fn cost(&self) -> usize {
        self.height + 1
    }
}

#[derive(Debug)]
pub(super) struct FreeIndex<A> {
    store: WordStore<A>,
    spans: usize,
    max_bitmap_words: [usize; BUDDY_ORDER_COUNT],
}

fn ceil(value: usize, divisor: usize) -> Option<usize> {
    Some(value / divisor + usize::from(!value.is_multiple_of(divisor)))
}

fn tree_words(bits: usize) -> Option<usize> {
    let mut words = ceil(bits, 64)?;
    let mut total = words;
    while words > 1 {
        words = ceil(words, 64)?;
        total = total.checked_add(words)?;
    }
    Some(total)
}

fn coordinates(first: usize, end: usize, order: usize) -> Option<(usize, usize)> {
    let alignment = 1usize.checked_shl(order.try_into().ok()?)?;
    let first = ceil(first, alignment)?;
    let end = ceil(end, alignment)?;
    Some((first, end.checked_sub(first)?))
}

impl<A: MemoryManagementArch> FreeIndex<A> {
    pub(super) fn requirements(areas: &[PhysMemoryArea]) -> Option<usize> {
        if areas.is_empty() {
            return None;
        }
        let mut words = areas.len().checked_mul(SPAN_WORDS)?;
        let mut previous_end = 0;
        for area in areas {
            let end = area.base.data().checked_add(area.size)?;
            if area.size == 0
                || !area.base.check_aligned(1 << MIN_ORDER)
                || end & ((1 << MIN_ORDER) - 1) != 0
                || area.base.data() < previous_end
            {
                return None;
            }
            previous_end = end;
            let first = area.base.data() >> MIN_ORDER;
            let end = end >> MIN_ORDER;
            words = words.checked_add(end - first)?;
            for order in 0..BUDDY_ORDER_COUNT {
                let (_, bits) = coordinates(first, end, order)?;
                words = words.checked_add(tree_words(bits)?)?;
            }
        }
        Some(words)
    }

    /// Areas describe the boot-time RAM upper bound. Pages subsequently
    /// reserved for this index simply retain zero head slots.
    pub(super) unsafe fn initialize(
        areas: &[PhysMemoryArea],
        mut allocate: impl FnMut() -> Option<PhysAddr>,
    ) -> Option<Self> {
        let words = Self::requirements(areas)?;
        let store = WordStore::<A>::new(words, &mut allocate)?;
        let mut cursor = areas.len().checked_mul(SPAN_WORDS)?;
        let mut max_bitmap_words = [0usize; BUDDY_ORDER_COUNT];
        for (index, area) in areas.iter().enumerate() {
            let descriptor = index * SPAN_WORDS;
            let first = area.base.data() >> MIN_ORDER;
            let end = area.base.data().checked_add(area.size)? >> MIN_ORDER;
            store.write(descriptor, first);
            store.write(descriptor + 1, end);
            store.write(descriptor + 2, cursor);
            cursor = cursor.checked_add(end - first)?;
            for (order, max_words) in max_bitmap_words.iter_mut().enumerate() {
                let (coordinate, bits) = coordinates(first, end, order)?;
                *max_words = (*max_words).max(ceil(bits, 64)?);
                let field = descriptor + 3 + order * 3;
                store.write(field, coordinate);
                store.write(field + 1, bits);
                store.write(field + 2, cursor);
                cursor = cursor.checked_add(tree_words(bits)?)?;
            }
        }
        assert_eq!(cursor, words);
        Some(Self {
            store,
            spans: areas.len(),
            max_bitmap_words,
        })
    }

    fn span(&self, index: usize) -> Span {
        let descriptor = index * SPAN_WORDS;
        // A descriptor is aligned to 512 bytes and never crosses a data page.
        let physical = self.store.physical(descriptor);
        let virtual_address = unsafe { A::phys_2_virt(physical).unwrap() };
        unsafe {
            Span {
                first: A::read(virtual_address),
                end: A::read(virtual_address + WORD_BYTES),
                slots: A::read(virtual_address + 2 * WORD_BYTES),
                descriptor,
            }
        }
    }

    fn locate(&self, pfn: usize) -> Option<Span> {
        let mut low = 0;
        let mut high = self.spans;
        while low < high {
            let middle = low + (high - low) / 2;
            let span = self.span(middle);
            if pfn < span.first {
                high = middle;
            } else if pfn >= span.end {
                low = middle + 1;
            } else {
                return Some(span);
            }
        }
        None
    }

    fn bitmap(&self, span: Span, order: u8) -> Bitmap {
        let relative = order as usize - MIN_ORDER;
        assert!(relative < BUDDY_ORDER_COUNT);
        let physical = self.store.physical(span.descriptor + 3 + relative * 3);
        let virtual_address = unsafe { A::phys_2_virt(physical).unwrap() };
        unsafe {
            Bitmap {
                first: A::read(virtual_address),
                bits: A::read(virtual_address + WORD_BYTES),
                offset: A::read(virtual_address + 2 * WORD_BYTES),
            }
        }
    }

    pub(super) fn lookup(&self, base: PhysAddr) -> Option<PhysAddr> {
        if !base.check_aligned(1 << MIN_ORDER) {
            return None;
        }
        let pfn = base.data() >> MIN_ORDER;
        let span = self.locate(pfn)?;
        let entry = self.store.read(span.slots + pfn - span.first);
        (entry != 0).then_some(PhysAddr::new(entry))
    }

    fn update_bit(&self, span: Span, base: PhysAddr, order: u8, set: bool) {
        let bitmap = self.bitmap(span, order);
        let mut bit = (base.data() >> order) - bitmap.first;
        assert!(bit < bitmap.bits);
        let mut words = ceil(bitmap.bits, 64).unwrap();
        let mut offset = bitmap.offset;
        loop {
            let address = self.store.physical(offset + bit / 64);
            let old: usize = unsafe { A::read(A::phys_2_virt(address).unwrap()) };
            let mask = 1usize << (bit % 64);
            let updated = if set { old | mask } else { old & !mask };
            assert_ne!(old, updated, "free index bitmap ownership mismatch");
            unsafe { A::write(A::phys_2_virt(address).unwrap(), updated) };
            if words == 1 || (old != 0) == (updated != 0) {
                return;
            }
            bit /= 64;
            offset += words;
            words = ceil(words, 64).unwrap();
        }
    }

    pub(super) fn insert(&self, base: PhysAddr, entry: PhysAddr, order: u8) {
        assert!(base.check_aligned(1usize << order));
        assert!(!entry.is_null());
        let pfn = base.data() >> MIN_ORDER;
        let span = self.locate(pfn).expect("free block outside managed index");
        let slot = self.store.physical(span.slots + pfn - span.first);
        let old: usize = unsafe { A::read(A::phys_2_virt(slot).unwrap()) };
        assert_eq!(old, 0, "duplicate free block head");
        unsafe { A::write(A::phys_2_virt(slot).unwrap(), entry.data()) };
        self.update_bit(span, base, order, true);
    }

    pub(super) fn remove(&self, base: PhysAddr, entry: PhysAddr, order: u8) {
        let pfn = base.data() >> MIN_ORDER;
        let span = self
            .locate(pfn)
            .expect("removed block outside managed index");
        let slot = self.store.physical(span.slots + pfn - span.first);
        let old: usize = unsafe { A::read(A::phys_2_virt(slot).unwrap()) };
        assert_eq!(
            old,
            entry.data(),
            "free index entry moved without relocation"
        );
        unsafe { A::write(A::phys_2_virt(slot).unwrap(), 0usize) };
        self.update_bit(span, base, order, false);
    }

    /// Moving the PageList slot does not change the block's address or order.
    pub(super) fn relocate(&self, base: PhysAddr, entry: PhysAddr) {
        let pfn = base.data() >> MIN_ORDER;
        let span = self
            .locate(pfn)
            .expect("relocated block outside managed index");
        let slot = self.store.physical(span.slots + pfn - span.first);
        let old: usize = unsafe { A::read(A::phys_2_virt(slot).unwrap()) };
        assert_ne!(old, 0, "relocated non-free block");
        unsafe { A::write(A::phys_2_virt(slot).unwrap(), entry.data()) };
    }

    fn charge(remaining: &mut usize, cost: usize) -> Result<(), super::compaction::RangeError> {
        if *remaining < cost {
            *remaining = 0;
            return Err(super::compaction::RangeError::Budget);
        }
        *remaining -= cost;
        Ok(())
    }

    fn find_bit(
        &self,
        offsets: &[usize],
        level: usize,
        low: usize,
        high: usize,
        remaining: &mut usize,
    ) -> Result<Option<usize>, super::compaction::RangeError> {
        if low >= high {
            return Ok(None);
        }
        let first_word = low / 64;
        let last_word = (high - 1) / 64;
        Self::charge(remaining, self.store.cost())?;
        let mut value = self.store.read(offsets[level] + first_word) & (usize::MAX << (low % 64));
        if first_word == last_word {
            value &= usize::MAX >> (63 - (high - 1) % 64);
        }
        if value != 0 {
            return Ok(Some(first_word * 64 + value.trailing_zeros() as usize));
        }
        if first_word == last_word {
            return Ok(None);
        }
        if first_word + 1 < last_word {
            let word = self.find_bit(offsets, level + 1, first_word + 1, last_word, remaining)?;
            if let Some(word) = word {
                Self::charge(remaining, self.store.cost())?;
                let value = self.store.read(offsets[level] + word);
                assert_ne!(value, 0, "free index summary references an empty word");
                return Ok(Some(word * 64 + value.trailing_zeros() as usize));
            }
        }
        Self::charge(remaining, self.store.cost())?;
        let value =
            self.store.read(offsets[level] + last_word) & (usize::MAX >> (63 - (high - 1) % 64));
        Ok((value != 0).then(|| last_word * 64 + value.trailing_zeros() as usize))
    }

    fn bitmap_offsets(bitmap: Bitmap) -> [usize; MAX_LEVELS] {
        let mut offsets = [0usize; MAX_LEVELS];
        offsets[0] = bitmap.offset;
        let mut level = 0;
        let mut words = ceil(bitmap.bits, 64).unwrap();
        while words > 1 {
            offsets[level + 1] = offsets[level] + words;
            words = ceil(words, 64).unwrap();
            level += 1;
        }
        offsets
    }

    pub(super) fn next_head(
        &self,
        order: u8,
        start: PhysAddr,
        end: PhysAddr,
        remaining: &mut usize,
    ) -> Result<Option<PhysAddr>, super::compaction::RangeError> {
        assert!((MIN_ORDER..MIN_ORDER + BUDDY_ORDER_COUNT).contains(&(order as usize)));
        if start >= end {
            return Ok(None);
        }
        let alignment = 1usize << order;
        let coordinate_start = ceil(start.data(), alignment).unwrap();
        let coordinate_end = ceil(end.data(), alignment).unwrap();
        let start_pfn = start.data() >> MIN_ORDER;
        let end_pfn = ceil(end.data(), 1 << MIN_ORDER).unwrap();
        // Binary-search the first intersecting span, then visit only RAM spans
        // in the address domain. No free-block chains or physical holes walk.
        let mut low = 0;
        let mut high = self.spans;
        while low < high {
            let mid = low + (high - low) / 2;
            Self::charge(remaining, self.store.cost())?;
            if self.span(mid).end <= start_pfn {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        for index in low..self.spans {
            Self::charge(remaining, self.store.cost())?;
            let span = self.span(index);
            if span.first >= end_pfn {
                break;
            }
            Self::charge(remaining, self.store.cost())?;
            let bitmap = self.bitmap(span, order);
            let first = coordinate_start.max(bitmap.first);
            let last = coordinate_end.min(bitmap.first + bitmap.bits);
            if first >= last {
                continue;
            }
            let offsets = Self::bitmap_offsets(bitmap);
            if let Some(bit) = self.find_bit(
                &offsets,
                0,
                first - bitmap.first,
                last - bitmap.first,
                remaining,
            )? {
                return Ok(Some(PhysAddr::new((bitmap.first + bit) << order)));
            }
        }
        Ok(None)
    }

    /// Count exact-order heads in [start, end), stopping at `limit`.
    /// Summaries locate nonempty leaf words; one popcount consumes every head
    /// in that word, instead of repeating a tree walk for each free block.
    pub(super) fn count_heads(
        &self,
        order: u8,
        start: PhysAddr,
        end: PhysAddr,
        limit: usize,
        remaining: &mut usize,
    ) -> Result<usize, super::compaction::RangeError> {
        assert!((MIN_ORDER..MIN_ORDER + BUDDY_ORDER_COUNT).contains(&(order as usize)));
        if limit == 0 || start >= end {
            return Ok(0);
        }
        let alignment = 1usize << order;
        let coordinate_start = ceil(start.data(), alignment).unwrap();
        let coordinate_end = ceil(end.data(), alignment).unwrap();
        let start_pfn = start.data() >> MIN_ORDER;
        let end_pfn = ceil(end.data(), 1 << MIN_ORDER).unwrap();
        let mut low = 0;
        let mut high = self.spans;
        while low < high {
            let middle = low + (high - low) / 2;
            Self::charge(remaining, self.store.cost())?;
            if self.span(middle).end <= start_pfn {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        let mut found = 0usize;
        for index in low..self.spans {
            Self::charge(remaining, self.store.cost())?;
            let span = self.span(index);
            if span.first >= end_pfn {
                break;
            }
            Self::charge(remaining, self.store.cost())?;
            let bitmap = self.bitmap(span, order);
            let first = coordinate_start.max(bitmap.first);
            let last = coordinate_end.min(bitmap.first + bitmap.bits);
            if first >= last {
                continue;
            }
            let first = first - bitmap.first;
            let last = last - bitmap.first;
            let offsets = Self::bitmap_offsets(bitmap);
            let leaf_words = ceil(bitmap.bits, 64).unwrap();
            let mut next_word = first / 64;
            let last_word = (last - 1) / 64;
            while next_word <= last_word {
                let word = if leaf_words == 1 {
                    0
                } else {
                    let Some(word) =
                        self.find_bit(&offsets, 1, next_word, last_word + 1, remaining)?
                    else {
                        break;
                    };
                    word
                };
                Self::charge(remaining, self.store.cost())?;
                let mut value = self.store.read(bitmap.offset + word);
                if word == first / 64 {
                    value &= usize::MAX << (first % 64);
                }
                if word == last_word {
                    value &= usize::MAX >> (63 - (last - 1) % 64);
                }
                found += (value.count_ones() as usize).min(limit - found);
                if found == limit {
                    return Ok(found);
                }
                next_word = word + 1;
            }
        }
        Ok(found)
    }

    /// Read-only local leaf census. This counts actual removal candidates,
    /// avoiding a worst-case assumption that every target PFN is a free head.
    /// The census itself is included in the returned prepayment; it never
    /// follows a PageList or changes bitmap/locator ownership.
    pub(super) fn isolation_bound(&self, start: PhysAddr, pages: usize) -> Option<usize> {
        if pages == 0 || !pages.is_power_of_two() || pages > 512 {
            return None;
        }
        let first_pfn = start.data() >> MIN_ORDER;
        let last_pfn = first_pfn.checked_add(pages)?;
        let target_order = pages.trailing_zeros() as usize;
        let cost = self.store.cost();
        let mut work = 0usize;
        let mut low = 0;
        let mut high = self.spans;
        while low < high {
            let middle = low + (high - low) / 2;
            work = work.checked_add(cost)?;
            if self.span(middle).end <= first_pfn {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        let mut heads = 0usize;
        let mut touched = [0usize; BUDDY_ORDER_COUNT];
        for span_index in low..self.spans {
            work = work.checked_add(cost)?;
            let span = self.span(span_index);
            if span.first >= last_pfn {
                break;
            }
            for (order, leaves) in touched.iter_mut().enumerate().take(target_order + 1) {
                work = work.checked_add(cost)?;
                let bitmap = self.bitmap(span, (MIN_ORDER + order) as u8);
                let alignment = 1usize << order;
                let first = ceil(first_pfn, alignment)?.max(bitmap.first);
                let end = ceil(last_pfn, alignment)?.min(bitmap.first + bitmap.bits);
                if first >= end {
                    continue;
                }
                let first = first - bitmap.first;
                let end = end - bitmap.first;
                let last_word = (end - 1) / 64;
                for word in first / 64..=last_word {
                    work = work.checked_add(cost)?;
                    let mut value = self.store.read(bitmap.offset + word);
                    if word == first / 64 {
                        value &= usize::MAX << (first % 64);
                    }
                    if word == last_word {
                        value &= usize::MAX >> (63 - (end - 1) % 64);
                    }
                    heads = heads.checked_add(value.count_ones() as usize)?;
                    *leaves += usize::from(value != 0);
                }
            }
        }
        // The executing PFN walk can inspect every page. Each removed head
        // then performs one locator removal and at most one last relocation.
        work = work.checked_add(pages.checked_mul(self.lookup_bound())?)?;
        work = work.checked_add(heads.checked_mul(2 * self.lookup_bound() + 2 * cost + 4)?)?;
        for (order, &leaves) in touched.iter().enumerate() {
            let mut words = self.max_bitmap_words[order];
            while words > 1 {
                // Each touched leaf can cause at most one update at this
                // level. Counting every leaf again is deliberately generous.
                work = work.checked_add(leaves.checked_mul(cost + 1)?)?;
                words = ceil(words, 64)?;
            }
        }
        Some(work)
    }

    pub(super) fn lookup_bound(&self) -> usize {
        let depth = usize::BITS as usize - self.spans.leading_zeros() as usize;
        (depth + 1) * self.store.cost() + 1
    }

    pub(super) fn point_mutation_bound(&self, points: usize) -> Option<usize> {
        let mut largest = self.max_bitmap_words.iter().copied().max().unwrap_or(0);
        let mut levels = 1;
        while largest > 1 {
            levels += 1;
            largest = ceil(largest, 64)?;
        }
        points.checked_mul(3 * self.lookup_bound() + (levels + 1) * (self.store.cost() + 1))
    }

    #[cfg(feature = "mm_selftest")]
    pub(super) fn fixture(areas: &[PhysMemoryArea]) -> Option<Self> {
        use crate::mm::allocator::page_frame::{allocate_page_frames, PageFrameCount};
        let mut owner = FixturePages(alloc::vec::Vec::new());
        let mut index = unsafe {
            Self::initialize(areas, || {
                let (address, count) = allocate_page_frames(PageFrameCount::ONE)?;
                assert_eq!(count, PageFrameCount::ONE);
                owner.0.push(address);
                Some(address)
            })?
        };
        index.store.owner = Some(owner);
        Some(index)
    }
}

#[cfg(feature = "mm_selftest")]
#[derive(Debug)]
struct FixturePages(alloc::vec::Vec<PhysAddr>);

#[cfg(feature = "mm_selftest")]
impl Drop for FixturePages {
    fn drop(&mut self) {
        use crate::mm::allocator::page_frame::{
            deallocate_page_frames, PageFrameCount, PhysPageFrame,
        };
        for address in self.0.drain(..) {
            unsafe { deallocate_page_frames(PhysPageFrame::new(address), PageFrameCount::ONE) };
        }
    }
}

#[cfg(all(
    feature = "mm_selftest",
    any(target_arch = "x86_64", target_arch = "riscv64")
))]
pub(super) fn selftest_checks() -> alloc::vec::Vec<(&'static str, bool)> {
    use crate::{arch::MMArch, mm::memblock::MemoryAreaAttr};
    let page = 1usize << MIN_ORDER;
    let low = PhysAddr::new(0x0200_0000);
    let high = PhysAddr::new((1usize << 32) + 0x0300_0000);
    let areas = [
        PhysMemoryArea::new(low, 8192 * page, MemoryAreaAttr::empty()),
        PhysMemoryArea::new(high, 4096 * page, MemoryAreaAttr::empty()),
    ];
    let Some(index) = FreeIndex::<MMArch>::fixture(&areas) else {
        return alloc::vec![("free_index_backing", false)];
    };
    let heads = [
        (low, MIN_ORDER as u8),
        (low + 2 * page, (MIN_ORDER + 1) as u8),
        (low + 512 * page, (MIN_ORDER + 9) as u8),
        (low + 4095 * page, MIN_ORDER as u8),
        (high, MIN_ORDER as u8),
        (high + 1024 * page, (MIN_ORDER + 8) as u8),
    ];
    for (ordinal, &(base, order)) in heads.iter().enumerate() {
        index.insert(base, PhysAddr::new(0x1000 + ordinal * WORD_BYTES), order);
    }
    let mut budget = 16384;
    let sparse = index.lookup(low + page).is_none()
        && index.lookup(PhysAddr::new(1usize << 32)).is_none()
        && index.next_head(MIN_ORDER as u8, low + page, high, &mut budget)
            == Ok(Some(low + 4095 * page))
        && index.next_head((MIN_ORDER + 8) as u8, low, high + 4096 * page, &mut budget)
            == Ok(Some(high + 1024 * page))
        && index.next_head((MIN_ORDER + 9) as u8, low, high, &mut budget)
            == Ok(Some(low + 512 * page));
    index.relocate(low, PhysAddr::new(0x3000));
    let relocated = index.lookup(low) == Some(PhysAddr::new(0x3000))
        && index.next_head(MIN_ORDER as u8, low, low + page, &mut budget) == Ok(Some(low));
    let mut zero = 0;
    let bounded = index.next_head(MIN_ORDER as u8, low, high, &mut zero)
        == Err(super::compaction::RangeError::Budget)
        && index.lookup(low) == Some(PhysAddr::new(0x3000));
    for (ordinal, &(base, order)) in heads.iter().enumerate() {
        let entry = if ordinal == 0 {
            0x3000
        } else {
            0x1000 + ordinal * WORD_BYTES
        };
        index.remove(base, PhysAddr::new(entry), order);
    }
    let cleared = heads.iter().all(|&(base, _)| index.lookup(base).is_none())
        && (MIN_ORDER..MIN_ORDER + BUDDY_ORDER_COUNT).all(|order| {
            index.next_head(order as u8, low, high + 4096 * page, &mut budget) == Ok(None)
        });
    let mut count_budget = 4096;
    for ordinal in 0..512 {
        index.insert(
            low + ordinal * 4 * page,
            PhysAddr::new(0x1000 + ordinal * WORD_BYTES),
            (MIN_ORDER + 2) as u8,
        );
    }
    let counted = index.count_heads(
        (MIN_ORDER + 2) as u8,
        low,
        low + 2048 * page,
        512,
        &mut count_budget,
    ) == Ok(512)
        && index.count_heads(
            (MIN_ORDER + 2) as u8,
            low + 1,
            low + 8 * page,
            512,
            &mut count_budget,
        ) == Ok(1)
        && index.count_heads(
            (MIN_ORDER + 2) as u8,
            low,
            high + 4096 * page,
            17,
            &mut count_budget,
        ) == Ok(17);
    let mut zero = 0;
    let count_bounded = index.count_heads((MIN_ORDER + 2) as u8, low, high, 0, &mut zero) == Ok(0)
        && index.count_heads((MIN_ORDER + 2) as u8, low, high, 1, &mut zero)
            == Err(super::compaction::RangeError::Budget);
    let mut reversed = areas;
    reversed.reverse();
    let invalid = FreeIndex::<MMArch>::requirements(&reversed).is_none()
        && FreeIndex::<MMArch>::requirements(&[PhysMemoryArea::new(
            low + 1,
            page,
            MemoryAreaAttr::empty(),
        )])
        .is_none()
        && FreeIndex::<MMArch>::requirements(&[PhysMemoryArea::new(
            PhysAddr::new(usize::MAX - page + 1),
            2 * page,
            MemoryAreaAttr::empty(),
        )])
        .is_none();
    alloc::vec![
        ("free_index_sparse_orders", sparse),
        ("free_index_relocation", relocated),
        ("free_index_budget_readonly", bounded),
        ("free_index_summary_clear", cleared),
        ("free_index_input_bounds", invalid),
        ("free_index_word_count", counted),
        ("free_index_count_budget", count_bounded),
    ]
}
