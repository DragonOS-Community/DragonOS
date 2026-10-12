//! Linux-compatible file-descriptor table storage.
//!
//! DragonOS does not currently have Linux's `kvmalloc()` fallback. Large
//! contiguous `Vec<Option<Arc<File>>>` allocations therefore become high-order
//! physically contiguous buddy allocations. This module keeps the common
//! 0..63 range inline and materializes the remaining descriptor space as a
//! fixed-depth page index. Slot storage uses independently allocated 4 KiB
//! blocks, which are order-0 pages on the currently runnable architectures.

use alloc::{sync::Arc, vec::Vec};
use bitmap::{static_bitmap, StaticBitmap};
use core::{
    mem::size_of,
    ops::{Deref, Index, IndexMut},
    sync::atomic::{AtomicUsize, Ordering},
};

use system_error::SystemError;

use crate::{
    cgroup::memory::{accounting::KernelByteKind, object::ObjectAccount, owned::OwnedBox},
    libs::rwsem::{RwSem, RwSemReadGuard},
    mm::allocation::MemoryAllocationError,
};

use super::file::{File, FileMode};

const INLINE_FDS: usize = usize::BITS as usize;
const BITS_PER_WORD: usize = usize::BITS as usize;
/// Bytes in one independently allocated descriptor-slot block.
///
/// This is a property of the fd-table layout, not of an architecture's
/// partially implemented page-size declaration. Keeping the block at 4 KiB
/// bounds every growth step while avoiding high-order buddy allocations.
const SLOT_BLOCK_BYTES: usize = 4096;
const CHUNK_SHIFT: usize = 9;
const CHUNK_FDS: usize = 1 << CHUNK_SHIFT;
const INDEX_SHIFT: usize = 6;
const INDEX_FANOUT: usize = 1 << INDEX_SHIFT;
const GROUP_SPAN: usize = CHUNK_FDS * INDEX_FANOUT * INDEX_FANOUT;
const ROOT_ENTRIES_MAX: usize = 1024;
const ROOT_BLOCK_ENTRIES: usize = 128;
const ROOT_BLOCKS: usize = ROOT_ENTRIES_MAX / ROOT_BLOCK_ENTRIES;
const DEFAULT_NR_OPEN: usize = 1024 * 1024;

static NR_OPEN: AtomicUsize = AtomicUsize::new(DEFAULT_NR_OPEN);
static NEXT_LOCK_OWNER_ID: AtomicUsize = AtomicUsize::new(1);

const _: () = {
    assert!(size_of::<Option<Arc<File>>>() * CHUNK_FDS == SLOT_BLOCK_BYTES);
    assert!(size_of::<static_bitmap!(CHUNK_FDS)>() == CHUNK_FDS / 8);
    assert!(size_of::<static_bitmap!(INDEX_FANOUT)>() == INDEX_FANOUT / 8);
    assert!(ROOT_ENTRIES_MAX * GROUP_SPAN > i32::MAX as usize);
};

#[inline]
fn alloc_lock_owner_id() -> usize {
    NEXT_LOCK_OWNER_ID.fetch_add(1, Ordering::Relaxed)
}

#[inline]
pub fn nr_open() -> usize {
    NR_OPEN.load(Ordering::Acquire)
}

#[inline]
pub fn nr_open_max() -> usize {
    let pointer_limit = usize::MAX / size_of::<Option<Arc<File>>>();
    core::cmp::min(i32::MAX as usize, pointer_limit) & !(BITS_PER_WORD - 1)
}

pub fn set_nr_open(value: usize) -> Result<(), SystemError> {
    if !(BITS_PER_WORD..=nr_open_max()).contains(&value) {
        return Err(SystemError::EINVAL);
    }
    NR_OPEN.store(value, Ordering::Release);
    Ok(())
}

#[inline]
fn bit_is_set(words: &[usize], bit: usize) -> bool {
    words[bit / BITS_PER_WORD] & (1usize << (bit % BITS_PER_WORD)) != 0
}

#[inline]
fn set_bit(words: &mut [usize], bit: usize, value: bool) {
    let mask = 1usize << (bit % BITS_PER_WORD);
    let word = &mut words[bit / BITS_PER_WORD];
    if value {
        *word |= mask;
    } else {
        *word &= !mask;
    }
}

fn prefix_is_full(words: &[usize], valid_bits: usize) -> bool {
    if valid_bits == 0 {
        return false;
    }
    let full_words = valid_bits / BITS_PER_WORD;
    if words[..full_words].iter().any(|word| *word != usize::MAX) {
        return false;
    }
    let tail = valid_bits % BITS_PER_WORD;
    tail == 0 || words[full_words] & ((1usize << tail) - 1) == (1usize << tail) - 1
}

fn find_zero_in_words(words: &[usize], start: usize, end: usize) -> Option<usize> {
    if start >= end {
        return None;
    }
    let first_word = start / BITS_PER_WORD;
    let last_word = (end - 1) / BITS_PER_WORD;
    for (word_index, word) in words
        .iter()
        .enumerate()
        .take(last_word + 1)
        .skip(first_word)
    {
        let word_base = word_index * BITS_PER_WORD;
        let lower = start.saturating_sub(word_base).min(BITS_PER_WORD);
        let upper = end.saturating_sub(word_base).min(BITS_PER_WORD);
        let lower_mask = usize::MAX << lower;
        let upper_mask = if upper == BITS_PER_WORD {
            usize::MAX
        } else {
            (1usize << upper) - 1
        };
        let candidates = !*word & lower_mask & upper_mask;
        if candidates != 0 {
            return Some(word_base + candidates.trailing_zeros() as usize);
        }
    }
    None
}

fn find_set_in_words(words: &[usize], start: usize, end: usize) -> Option<usize> {
    if start >= end {
        return None;
    }
    let first_word = start / BITS_PER_WORD;
    let last_word = (end - 1) / BITS_PER_WORD;
    for (word_index, word) in words
        .iter()
        .enumerate()
        .take(last_word + 1)
        .skip(first_word)
    {
        let word_base = word_index * BITS_PER_WORD;
        let lower = start.saturating_sub(word_base).min(BITS_PER_WORD);
        let upper = end.saturating_sub(word_base).min(BITS_PER_WORD);
        let lower_mask = usize::MAX << lower;
        let upper_mask = if upper == BITS_PER_WORD {
            usize::MAX
        } else {
            (1usize << upper) - 1
        };
        let candidates = *word & lower_mask & upper_mask;
        if candidates != 0 {
            return Some(word_base + candidates.trailing_zeros() as usize);
        }
    }
    None
}

#[derive(Debug)]
struct FdChunk {
    slots: OwnedBox<[Option<Arc<File>>; CHUNK_FDS]>,
    open_fds: static_bitmap!(CHUNK_FDS),
    close_on_exec: static_bitmap!(CHUNK_FDS),
}

impl FdChunk {
    fn try_new(creator: &Arc<ObjectAccount>) -> Result<OwnedBox<Self>, MemoryAllocationError> {
        let slots = OwnedBox::prepare(creator, KernelByteKind::SlabUnreclaimable)?
            .try_allocate()
            .map_err(|_| SystemError::ENOMEM)?
            .init_array(|_| None);
        Ok(
            OwnedBox::prepare(creator, KernelByteKind::SlabUnreclaimable)?
                .try_init(Self {
                    slots,
                    open_fds: StaticBitmap::new(),
                    close_on_exec: StaticBitmap::new(),
                })
                .map_err(|_| SystemError::ENOMEM)?,
        )
    }

    fn is_full(&self, valid_slots: usize) -> bool {
        prefix_is_full(&self.open_fds.data, valid_slots)
    }
}

#[derive(Debug)]
struct FdDirectory {
    chunks: [Option<OwnedBox<FdChunk>>; INDEX_FANOUT],
    present_chunks: static_bitmap!(INDEX_FANOUT),
    full_chunks: static_bitmap!(INDEX_FANOUT),
}

impl FdDirectory {
    fn try_new(creator: &Arc<ObjectAccount>) -> Result<OwnedBox<Self>, MemoryAllocationError> {
        Ok(
            OwnedBox::prepare(creator, KernelByteKind::SlabUnreclaimable)?
                .try_init(Self {
                    chunks: core::array::from_fn(|_| None),
                    present_chunks: StaticBitmap::new(),
                    full_chunks: StaticBitmap::new(),
                })
                .map_err(|_| SystemError::ENOMEM)?,
        )
    }
}

#[derive(Debug)]
struct FdGroup {
    directories: [Option<OwnedBox<FdDirectory>>; INDEX_FANOUT],
    present_directories: static_bitmap!(INDEX_FANOUT),
    full_directories: static_bitmap!(INDEX_FANOUT),
}

const _: () = {
    assert!(size_of::<FdDirectory>() > 2048);
    assert!(size_of::<FdDirectory>() <= SLOT_BLOCK_BYTES);
    assert!(size_of::<FdGroup>() > 2048);
    assert!(size_of::<FdGroup>() <= SLOT_BLOCK_BYTES);
};

impl FdGroup {
    fn try_new(creator: &Arc<ObjectAccount>) -> Result<OwnedBox<Self>, MemoryAllocationError> {
        Ok(
            OwnedBox::prepare(creator, KernelByteKind::SlabUnreclaimable)?
                .try_init(Self {
                    directories: core::array::from_fn(|_| None),
                    present_directories: StaticBitmap::new(),
                    full_directories: StaticBitmap::new(),
                })
                .map_err(|_| SystemError::ENOMEM)?,
        )
    }
}

type RootBlock = [Option<OwnedBox<FdGroup>>; ROOT_BLOCK_ENTRIES];

#[derive(Debug)]
struct PagedRoots {
    blocks: [Option<OwnedBox<RootBlock>>; ROOT_BLOCKS],
    len: usize,
}

impl Default for PagedRoots {
    fn default() -> Self {
        Self {
            blocks: core::array::from_fn(|_| None),
            len: 0,
        }
    }
}

impl PagedRoots {
    fn len(&self) -> usize {
        self.len
    }
    fn get(&self, index: usize) -> Option<&Option<OwnedBox<FdGroup>>> {
        if index >= self.len {
            return None;
        }
        Some(&self.blocks[index / ROOT_BLOCK_ENTRIES].as_ref()?[index % ROOT_BLOCK_ENTRIES])
    }
    fn get_mut(&mut self, index: usize) -> Option<&mut Option<OwnedBox<FdGroup>>> {
        if index >= self.len {
            return None;
        }
        Some(&mut self.blocks[index / ROOT_BLOCK_ENTRIES].as_mut()?[index % ROOT_BLOCK_ENTRIES])
    }
}
impl Index<usize> for PagedRoots {
    type Output = Option<OwnedBox<FdGroup>>;
    fn index(&self, index: usize) -> &Self::Output {
        self.get(index).expect("materialized root block")
    }
}
impl IndexMut<usize> for PagedRoots {
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        self.get_mut(index).expect("materialized root block")
    }
}

const _: () = {
    assert!(size_of::<RootBlock>() == SLOT_BLOCK_BYTES);
    assert!(ROOT_BLOCKS == 8);
};

#[derive(Debug, Default)]
struct PagedFdStorage {
    groups: PagedRoots,
    present_groups: static_bitmap!(ROOT_ENTRIES_MAX),
    full_groups: static_bitmap!(ROOT_ENTRIES_MAX),
}

impl PagedFdStorage {
    fn ensure_root_len(&mut self, required: usize) {
        assert!(required <= ROOT_ENTRIES_MAX);
        self.groups.len = self.groups.len.max(required);
    }
}

#[derive(Debug)]
struct PreparedPath {
    group_index: usize,
    directory_index: usize,
    chunk_index: usize,
    root: Option<OwnedBox<RootBlock>>,
    group: Option<OwnedBox<FdGroup>>,
    directory: Option<OwnedBox<FdDirectory>>,
    chunk: Option<OwnedBox<FdChunk>>,
}

#[derive(Clone, Copy)]
struct PathNeed {
    group_index: usize,
    directory_index: usize,
    chunk_index: usize,
    root: bool,
    group: bool,
    directory: bool,
}

impl PreparedPath {
    fn try_new(
        need: PathNeed,
        creator: &Arc<ObjectAccount>,
    ) -> Result<Self, MemoryAllocationError> {
        Ok(Self {
            group_index: need.group_index,
            directory_index: need.directory_index,
            chunk_index: need.chunk_index,
            root: if need.root {
                Some(
                    OwnedBox::<RootBlock>::prepare(creator, KernelByteKind::SlabUnreclaimable)?
                        .try_allocate()
                        .map_err(|_| SystemError::ENOMEM)?
                        .init_array(|_| None),
                )
            } else {
                None
            },
            group: if need.group {
                Some(FdGroup::try_new(creator)?)
            } else {
                None
            },
            directory: if need.directory {
                Some(FdDirectory::try_new(creator)?)
            } else {
                None
            },
            chunk: Some(FdChunk::try_new(creator)?),
        })
    }
}

#[derive(Debug)]
pub struct DroppedFd {
    file: Arc<File>,
    lock_owner_id: usize,
}

impl DroppedFd {
    pub fn new(file: Arc<File>, lock_owner_id: usize) -> Self {
        Self {
            file,
            lock_owner_id,
        }
    }

    pub fn finish_close(self) -> Result<(), SystemError> {
        let flush_result = self.file.flush_for_close(self.lock_owner_id as u64);
        super::posix_lock::release_posix_for_file_owner(&self.file, self.lock_owner_id);
        flush_result
    }
}

pub(crate) struct FdRangeScan {
    pub(crate) next: usize,
    pub(crate) scanned: usize,
    pub(crate) dropped: Option<DroppedFd>,
    pub(crate) done: bool,
}

#[derive(Debug)]
pub struct FdTableState {
    inline_slots: [Option<Arc<File>>; INLINE_FDS],
    inline_open: usize,
    inline_cloexec: usize,
    paged: Option<PagedFdStorage>,
    logical_capacity: usize,
    next_fd: usize,
    content_generation: u64,
    lock_owner_id: usize,
}

impl Default for FdTableState {
    fn default() -> Self {
        Self::new()
    }
}

impl FdTableState {
    pub fn new() -> Self {
        Self {
            inline_slots: core::array::from_fn(|_| None),
            inline_open: 0,
            inline_cloexec: 0,
            paged: None,
            logical_capacity: INLINE_FDS,
            next_fd: 0,
            content_generation: 0,
            lock_owner_id: alloc_lock_owner_id(),
        }
    }

    #[inline]
    fn bump_generation(&mut self) {
        self.content_generation = self.content_generation.wrapping_add(1);
    }

    #[inline]
    fn indexes(fd: usize) -> (usize, usize, usize, usize) {
        debug_assert!(fd >= INLINE_FDS);
        let relative = fd - INLINE_FDS;
        let slot_index = relative & (CHUNK_FDS - 1);
        let chunk_number = relative >> CHUNK_SHIFT;
        let chunk_index = chunk_number & (INDEX_FANOUT - 1);
        let directory_number = chunk_number >> INDEX_SHIFT;
        let directory_index = directory_number & (INDEX_FANOUT - 1);
        let group_index = directory_number >> INDEX_SHIFT;
        (group_index, directory_index, chunk_index, slot_index)
    }

    #[inline]
    fn chunk_number(fd: usize) -> usize {
        (fd - INLINE_FDS) >> CHUNK_SHIFT
    }

    #[inline]
    fn chunk_base(chunk_number: usize) -> usize {
        INLINE_FDS + (chunk_number << CHUNK_SHIFT)
    }

    fn chunk(&self, fd: usize) -> Option<&FdChunk> {
        if fd < INLINE_FDS {
            return None;
        }
        let (group, directory, chunk, _) = Self::indexes(fd);
        self.paged
            .as_ref()?
            .groups
            .get(group)?
            .as_deref()?
            .directories[directory]
            .as_deref()?
            .chunks[chunk]
            .as_deref()
    }

    fn chunk_mut(&mut self, fd: usize) -> Option<&mut FdChunk> {
        if fd < INLINE_FDS {
            return None;
        }
        let (group, directory, chunk, _) = Self::indexes(fd);
        self.paged
            .as_mut()?
            .groups
            .get_mut(group)?
            .as_deref_mut()?
            .directories[directory]
            .as_deref_mut()?
            .chunks[chunk]
            .as_deref_mut()
    }

    fn is_open(&self, fd: usize) -> bool {
        if fd < INLINE_FDS {
            return self.inline_open & (1usize << fd) != 0;
        }
        let (_, _, _, slot) = Self::indexes(fd);
        self.chunk(fd)
            .is_some_and(|chunk| bit_is_set(&chunk.open_fds.data, slot))
    }

    fn file_ref(&self, fd: usize) -> Option<&Arc<File>> {
        if fd >= self.logical_capacity {
            return None;
        }
        if fd < INLINE_FDS {
            return self.inline_slots[fd].as_ref();
        }
        let (_, _, _, slot) = Self::indexes(fd);
        self.chunk(fd)?.slots[slot].as_ref()
    }

    fn set_slot(
        &mut self,
        fd: usize,
        file: Option<Arc<File>>,
        open: bool,
        cloexec: bool,
    ) -> Option<Arc<File>> {
        let old = if fd < INLINE_FDS {
            let old = core::mem::replace(&mut self.inline_slots[fd], file);
            let mask = 1usize << fd;
            if open {
                self.inline_open |= mask;
            } else {
                self.inline_open &= !mask;
            }
            if cloexec {
                self.inline_cloexec |= mask;
            } else {
                self.inline_cloexec &= !mask;
            }
            old
        } else {
            let (_, _, _, slot) = Self::indexes(fd);
            let chunk = self
                .chunk_mut(fd)
                .expect("fd slot must be materialized before mutation");
            let old = core::mem::replace(&mut chunk.slots[slot], file);
            set_bit(&mut chunk.open_fds.data, slot, open);
            set_bit(&mut chunk.close_on_exec.data, slot, cloexec);
            old
        };
        self.refresh_summary_for_fd(fd);
        old
    }

    fn get_cloexec_raw(&self, fd: usize) -> bool {
        if fd >= self.logical_capacity {
            return false;
        }
        if fd < INLINE_FDS {
            return self.inline_cloexec & (1usize << fd) != 0;
        }
        let (_, _, _, slot) = Self::indexes(fd);
        self.chunk(fd)
            .is_some_and(|chunk| bit_is_set(&chunk.close_on_exec.data, slot))
    }

    fn set_cloexec_raw(&mut self, fd: usize, value: bool) {
        if fd < INLINE_FDS {
            let mask = 1usize << fd;
            if value {
                self.inline_cloexec |= mask;
            } else {
                self.inline_cloexec &= !mask;
            }
            return;
        }
        let (_, _, _, slot) = Self::indexes(fd);
        if let Some(chunk) = self.chunk_mut(fd) {
            set_bit(&mut chunk.close_on_exec.data, slot, value);
        }
    }

    fn valid_slots_in_chunk(&self, chunk_number: usize) -> usize {
        self.logical_capacity
            .saturating_sub(Self::chunk_base(chunk_number))
            .min(CHUNK_FDS)
    }

    fn valid_chunks_in_directory(&self, directory_number: usize) -> usize {
        let first_fd = INLINE_FDS + directory_number * INDEX_FANOUT * CHUNK_FDS;
        let valid_fds = self.logical_capacity.saturating_sub(first_fd);
        valid_fds
            .saturating_add(CHUNK_FDS - 1)
            .checked_div(CHUNK_FDS)
            .unwrap_or(0)
            .min(INDEX_FANOUT)
    }

    fn valid_directories_in_group(&self, group_index: usize) -> usize {
        let first_fd = INLINE_FDS + group_index * GROUP_SPAN;
        let directory_span = INDEX_FANOUT * CHUNK_FDS;
        let valid_fds = self.logical_capacity.saturating_sub(first_fd);
        valid_fds
            .saturating_add(directory_span - 1)
            .checked_div(directory_span)
            .unwrap_or(0)
            .min(INDEX_FANOUT)
    }

    fn refresh_summary_for_fd(&mut self, fd: usize) {
        if fd < INLINE_FDS || self.paged.is_none() {
            return;
        }
        let (group_index, directory_index, chunk_index, _) = Self::indexes(fd);
        let chunk_number = Self::chunk_number(fd);
        let valid_slots = self.valid_slots_in_chunk(chunk_number);
        let valid_chunks =
            self.valid_chunks_in_directory(group_index * INDEX_FANOUT + directory_index);
        let valid_directories = self.valid_directories_in_group(group_index);

        let paged = self.paged.as_mut().unwrap();
        let group = paged.groups[group_index].as_deref_mut().unwrap();
        let directory = group.directories[directory_index].as_deref_mut().unwrap();
        let chunk_full = directory.chunks[chunk_index]
            .as_deref()
            .is_some_and(|chunk| chunk.is_full(valid_slots));
        set_bit(&mut directory.full_chunks.data, chunk_index, chunk_full);
        let directory_full = prefix_is_full(&directory.full_chunks.data, valid_chunks);
        set_bit(
            &mut group.full_directories.data,
            directory_index,
            directory_full,
        );
        let group_full = prefix_is_full(&group.full_directories.data, valid_directories);
        set_bit(&mut paged.full_groups.data, group_index, group_full);
    }

    fn rebuild_summaries(&mut self) {
        let Some(mut paged) = self.paged.take() else {
            return;
        };
        paged.present_groups = StaticBitmap::new();
        paged.full_groups = StaticBitmap::new();
        for group_index in 0..paged.groups.len() {
            let Some(group) = paged
                .groups
                .get_mut(group_index)
                .and_then(Option::as_deref_mut)
            else {
                continue;
            };
            set_bit(&mut paged.present_groups.data, group_index, true);
            group.present_directories = StaticBitmap::new();
            group.full_directories = StaticBitmap::new();
            for directory_index in 0..INDEX_FANOUT {
                let Some(directory) = group.directories[directory_index].as_deref_mut() else {
                    continue;
                };
                set_bit(&mut group.present_directories.data, directory_index, true);
                directory.present_chunks = StaticBitmap::new();
                directory.full_chunks = StaticBitmap::new();
                for chunk_index in 0..INDEX_FANOUT {
                    let Some(chunk) = directory.chunks[chunk_index].as_deref() else {
                        continue;
                    };
                    set_bit(&mut directory.present_chunks.data, chunk_index, true);
                    let chunk_number =
                        (group_index * INDEX_FANOUT + directory_index) * INDEX_FANOUT + chunk_index;
                    let valid = self.valid_slots_in_chunk(chunk_number);
                    set_bit(
                        &mut directory.full_chunks.data,
                        chunk_index,
                        chunk.is_full(valid),
                    );
                }
                let directory_number = group_index * INDEX_FANOUT + directory_index;
                let valid = self.valid_chunks_in_directory(directory_number);
                set_bit(
                    &mut group.full_directories.data,
                    directory_index,
                    prefix_is_full(&directory.full_chunks.data, valid),
                );
            }
            let valid = self.valid_directories_in_group(group_index);
            set_bit(
                &mut paged.full_groups.data,
                group_index,
                prefix_is_full(&group.full_directories.data, valid),
            );
        }
        self.paged = Some(paged);
    }

    fn path_need(
        &self,
        fd: usize,
        group_already_prepared: bool,
        directory_already_prepared: bool,
        root_already_prepared: bool,
    ) -> PathNeed {
        let (group_index, directory_index, chunk_index, _) = Self::indexes(fd);
        let group_exists = group_already_prepared
            || self
                .paged
                .as_ref()
                .and_then(|paged| paged.groups.get(group_index))
                .and_then(Option::as_deref)
                .is_some();
        let directory_exists = directory_already_prepared
            || self
                .paged
                .as_ref()
                .and_then(|paged| paged.groups.get(group_index))
                .and_then(Option::as_deref)
                .and_then(|group| group.directories[directory_index].as_deref())
                .is_some();
        PathNeed {
            group_index,
            directory_index,
            chunk_index,
            root: !root_already_prepared
                && self.paged.as_ref().is_none_or(|paged| {
                    paged.groups.blocks[group_index / ROOT_BLOCK_ENTRIES].is_none()
                }),
            group: !group_exists,
            directory: !directory_exists,
        }
    }

    fn commit_paths(&mut self, paths: &mut [Option<PreparedPath>]) {
        let required_root = paths
            .iter()
            .filter_map(Option::as_ref)
            .map(|path| path.group_index + 1)
            .max()
            .unwrap_or(0);
        if required_root == 0 {
            return;
        }

        let paged = match &mut self.paged {
            Some(paged) => {
                paged.ensure_root_len(required_root);
                paged
            }
            slot @ None => {
                let mut paged = PagedFdStorage::default();
                paged.ensure_root_len(required_root);
                slot.insert(paged)
            }
        };
        for path in paths.iter_mut().filter_map(Option::as_mut) {
            let block = &mut paged.groups.blocks[path.group_index / ROOT_BLOCK_ENTRIES];
            if block.is_none() {
                *block = Some(path.root.take().expect("prepared root block"));
            }
            if paged.groups[path.group_index].is_none() {
                paged.groups[path.group_index] = path.group.take();
            }
            set_bit(&mut paged.present_groups.data, path.group_index, true);
            let group = paged.groups[path.group_index].as_deref_mut().unwrap();
            if group.directories[path.directory_index].is_none() {
                group.directories[path.directory_index] = path.directory.take();
            }
            set_bit(
                &mut group.present_directories.data,
                path.directory_index,
                true,
            );
            let directory = group.directories[path.directory_index]
                .as_deref_mut()
                .unwrap();
            if directory.chunks[path.chunk_index].is_none() {
                directory.chunks[path.chunk_index] = path.chunk.take();
            }
            set_bit(&mut directory.present_chunks.data, path.chunk_index, true);
        }
    }

    fn planned_capacity(&self, required: usize) -> Result<usize, SystemError> {
        if required <= self.logical_capacity {
            return Ok(self.logical_capacity);
        }
        Self::planned_capacity_from_inline(required)
    }

    fn planned_capacity_from_inline(required: usize) -> Result<usize, SystemError> {
        if required <= INLINE_FDS {
            return Ok(INLINE_FDS);
        }
        let mut target = required
            .checked_next_power_of_two()
            .ok_or(SystemError::EMFILE)?;
        let limit = nr_open();
        if target > limit {
            target = limit & !(BITS_PER_WORD - 1);
        }
        if target < required {
            return Err(SystemError::EMFILE);
        }
        Ok(target)
    }

    fn collect_path_needs(&self, fds: &[usize], paths: &mut [Option<PathNeed>]) {
        let mut length = 0;
        for &fd in fds {
            if fd < INLINE_FDS || self.chunk(fd).is_some() {
                continue;
            }
            let (group, directory, chunk, _) = Self::indexes(fd);
            if paths.iter().flatten().any(|path| {
                path.group_index == group
                    && path.directory_index == directory
                    && path.chunk_index == chunk
            }) {
                continue;
            }
            let group_prepared = paths.iter().flatten().any(|path| path.group_index == group);
            let directory_prepared = paths
                .iter()
                .flatten()
                .any(|path| path.group_index == group && path.directory_index == directory);
            let root_prepared = paths
                .iter()
                .flatten()
                .any(|path| path.group_index / ROOT_BLOCK_ENTRIES == group / ROOT_BLOCK_ENTRIES);
            assert!(length < paths.len());
            paths[length] =
                Some(self.path_need(fd, group_prepared, directory_prepared, root_prepared));
            length += 1;
        }
    }

    /// Only used for a private, unpublished clone. Public mutations prepare
    /// these allocations outside the table guard and revalidate generation.
    fn ensure_fds_materialized(
        &mut self,
        fds: &[usize],
        creator: &Arc<ObjectAccount>,
    ) -> Result<(), MemoryAllocationError> {
        let required = fds
            .iter()
            .copied()
            .max()
            .and_then(|fd| fd.checked_add(1))
            .unwrap_or(INLINE_FDS);
        let target_capacity = self.planned_capacity(required)?;
        let mut needs = Vec::new();
        needs
            .try_reserve_exact(fds.len())
            .map_err(|_| SystemError::ENOMEM)?;
        needs.resize(fds.len(), None);
        self.collect_path_needs(fds, &mut needs);
        let mut paths = Vec::new();
        paths
            .try_reserve_exact(fds.len())
            .map_err(|_| SystemError::ENOMEM)?;
        for need in needs.into_iter().flatten() {
            paths.push(Some(PreparedPath::try_new(need, creator)?));
        }
        self.commit_paths(&mut paths);
        if target_capacity != self.logical_capacity {
            self.logical_capacity = target_capacity;
            self.rebuild_summaries();
        }
        Ok(())
    }

    fn find_next_free(&self, start: usize, end: usize) -> Option<usize> {
        let end = end.min(nr_open_max().saturating_add(1));
        if start >= end {
            return None;
        }
        if start < INLINE_FDS {
            let inline_end = end.min(INLINE_FDS);
            let words = [self.inline_open];
            if let Some(bit) = find_zero_in_words(&words, start, inline_end) {
                return Some(bit);
            }
        }

        let mut fd = start.max(INLINE_FDS);
        while fd < end {
            let (group_index, directory_index, chunk_index, _) = Self::indexes(fd);
            let group_base = INLINE_FDS + group_index * GROUP_SPAN;
            let group_end = end.min(group_base.saturating_add(GROUP_SPAN));
            let Some(paged) = self.paged.as_ref() else {
                return Some(fd);
            };
            if !bit_is_set(&paged.present_groups.data, group_index) {
                return Some(fd);
            }
            let group = paged.groups[group_index]
                .as_deref()
                .expect("present group summary must match storage");
            if group_end <= self.logical_capacity
                && bit_is_set(&paged.full_groups.data, group_index)
            {
                fd = group_end;
                continue;
            }

            let directory_span = INDEX_FANOUT * CHUNK_FDS;
            let directory_base = group_base + directory_index * directory_span;
            let directory_end = group_end.min(directory_base.saturating_add(directory_span));
            if !bit_is_set(&group.present_directories.data, directory_index) {
                return Some(fd);
            }
            let directory = group.directories[directory_index]
                .as_deref()
                .expect("present directory summary must match storage");
            if directory_end <= self.logical_capacity
                && bit_is_set(&group.full_directories.data, directory_index)
            {
                fd = directory_end;
                continue;
            }

            let chunk_base = directory_base + chunk_index * CHUNK_FDS;
            let chunk_end = directory_end.min(chunk_base.saturating_add(CHUNK_FDS));
            if !bit_is_set(&directory.present_chunks.data, chunk_index) {
                return Some(fd);
            }
            let chunk = directory.chunks[chunk_index]
                .as_deref()
                .expect("present chunk summary must match storage");
            if chunk_end <= self.logical_capacity
                && bit_is_set(&directory.full_chunks.data, chunk_index)
            {
                fd = chunk_end;
                continue;
            }
            if let Some(slot) = find_zero_in_words(
                &chunk.open_fds.data,
                fd - chunk_base,
                chunk_end - chunk_base,
            ) {
                return Some(chunk_base + slot);
            }
            fd = chunk_end;
        }
        None
    }

    fn find_next_installed(&self, start: usize, end: usize) -> Option<usize> {
        let mut fd = start;
        while fd < end.min(self.logical_capacity) {
            if fd < INLINE_FDS {
                let available = self.inline_open & (usize::MAX << fd);
                if available != 0 {
                    let candidate = available.trailing_zeros() as usize;
                    if candidate < end && self.inline_slots[candidate].is_some() {
                        return Some(candidate);
                    }
                    fd = candidate.saturating_add(1);
                    continue;
                }
                fd = INLINE_FDS;
                continue;
            }
            let (group_index, directory_index, chunk_index, _) = Self::indexes(fd);
            let group_base = INLINE_FDS + group_index * GROUP_SPAN;
            let group_end = end
                .min(self.logical_capacity)
                .min(group_base.saturating_add(GROUP_SPAN));
            let Some(paged) = self.paged.as_ref() else {
                fd = group_end;
                continue;
            };
            if !bit_is_set(&paged.present_groups.data, group_index) {
                fd = group_end;
                continue;
            }
            let group = paged.groups[group_index]
                .as_deref()
                .expect("present group summary must match storage");

            let directory_span = INDEX_FANOUT * CHUNK_FDS;
            let directory_base = group_base + directory_index * directory_span;
            let directory_end = group_end.min(directory_base.saturating_add(directory_span));
            if !bit_is_set(&group.present_directories.data, directory_index) {
                fd = directory_end;
                continue;
            }
            let directory = group.directories[directory_index]
                .as_deref()
                .expect("present directory summary must match storage");

            let chunk_base = directory_base + chunk_index * CHUNK_FDS;
            let chunk_end = directory_end.min(chunk_base.saturating_add(CHUNK_FDS));
            if !bit_is_set(&directory.present_chunks.data, chunk_index) {
                fd = chunk_end;
                continue;
            }
            let chunk = directory.chunks[chunk_index]
                .as_deref()
                .expect("present chunk summary must match storage");
            let mut cursor = fd - chunk_base;
            while cursor < chunk_end - chunk_base {
                let word_index = cursor / BITS_PER_WORD;
                let lower = cursor % BITS_PER_WORD;
                let mut candidates = chunk.open_fds.data[word_index] & (usize::MAX << lower);
                while candidates != 0 {
                    let slot = word_index * BITS_PER_WORD + candidates.trailing_zeros() as usize;
                    if slot >= chunk_end - chunk_base {
                        break;
                    }
                    if chunk.slots[slot].is_some() {
                        return Some(chunk_base + slot);
                    }
                    candidates &= candidates - 1;
                }
                cursor = (word_index + 1) * BITS_PER_WORD;
            }
            fd = chunk_end;
        }
        None
    }

    fn update_next_fd_after_open(&mut self, fd: usize) {
        if fd != self.next_fd {
            return;
        }
        self.next_fd = self
            .find_next_free(fd.saturating_add(1), self.logical_capacity)
            .unwrap_or(self.logical_capacity);
    }

    fn select_reservation<const N: usize>(
        &self,
        soft_limit: usize,
        min_fd: usize,
        _cloexec: bool,
    ) -> Result<[i32; N], SystemError> {
        let end = soft_limit.min(nr_open_max().saturating_add(1));
        let mut result = [0i32; N];
        let mut cursor = self.next_fd.max(min_fd);
        for slot in result.iter_mut() {
            let fd = self
                .find_next_free(cursor, end)
                .ok_or(SystemError::EMFILE)?;
            *slot = i32::try_from(fd).map_err(|_| SystemError::EMFILE)?;
            cursor = fd.checked_add(1).ok_or(SystemError::EMFILE)?;
        }
        Ok(result)
    }

    fn commit_reservation<const N: usize>(&mut self, result: [i32; N], cloexec: bool) {
        let fds = result.map(|fd| fd as usize);
        for fd in fds {
            self.set_slot(fd, None, true, cloexec);
            self.update_next_fd_after_open(fd);
        }
        self.bump_generation();
    }

    fn release_reserved(&mut self, fd: usize) {
        if fd >= self.logical_capacity || !self.is_open(fd) || self.file_ref(fd).is_some() {
            return;
        }
        self.set_slot(fd, None, false, false);
        self.next_fd = self.next_fd.min(fd);
        self.bump_generation();
    }

    fn install_reserved(&mut self, fd: usize, file: Arc<File>) -> Result<(), SystemError> {
        if fd >= self.logical_capacity || !self.is_open(fd) || self.file_ref(fd).is_some() {
            return Err(SystemError::EBADF);
        }
        let cloexec = self.get_cloexec_raw(fd);
        self.set_slot(fd, Some(file), true, cloexec);
        self.bump_generation();
        Ok(())
    }

    fn replace_arc(
        &mut self,
        file: Arc<File>,
        fd: i32,
        cloexec: bool,
        soft_limit: usize,
    ) -> Result<(i32, Option<DroppedFd>), SystemError> {
        if fd < 0 || fd as usize >= soft_limit {
            return Err(SystemError::EMFILE);
        }
        let index = fd as usize;
        if self.is_open(index) && self.file_ref(index).is_none() {
            return Err(SystemError::EBUSY);
        }
        let old = self.set_slot(index, Some(file), true, cloexec);
        self.update_next_fd_after_open(index);
        self.bump_generation();
        Ok((fd, old.map(|file| DroppedFd::new(file, self.lock_owner_id))))
    }

    pub fn fd_open_count(&self) -> usize {
        let mut count = 0;
        let mut cursor = 0;
        while let Some(fd) = self.find_next_installed(cursor, self.logical_capacity) {
            count += 1;
            cursor = fd + 1;
        }
        count
    }

    pub fn get_file_by_fd(&self, fd: i32) -> Option<Arc<File>> {
        if fd < 0 {
            return None;
        }
        self.file_ref(fd as usize).cloned()
    }

    pub fn get_file_by_fd_not_raw(&self, fd: i32, mask: FileMode) -> Option<Arc<File>> {
        self.get_file_by_fd(fd)
            .filter(|file| !file.mode().contains(mask))
    }

    pub fn get_cloexec(&self, fd: i32) -> bool {
        fd >= 0 && self.get_cloexec_raw(fd as usize)
    }

    pub fn lock_owner_id(&self) -> usize {
        self.lock_owner_id
    }

    pub(crate) fn close_range_end(&self, last: u32) -> Option<usize> {
        self.logical_capacity
            .checked_sub(1)
            .map(|end| end.min(last as usize))
    }

    fn drop_fd(&mut self, fd: i32) -> Result<DroppedFd, SystemError> {
        let index = usize::try_from(fd).map_err(|_| SystemError::EBADF)?;
        let file = self.file_ref(index).cloned().ok_or(SystemError::EBADF)?;
        self.set_slot(index, None, false, false);
        self.next_fd = self.next_fd.min(index);
        self.bump_generation();
        Ok(DroppedFd::new(file, self.lock_owner_id))
    }

    fn take_next_open_in_range(
        &mut self,
        cursor: usize,
        end: usize,
        scan_budget: usize,
    ) -> FdRangeScan {
        debug_assert!(scan_budget > 0);
        if cursor > end || cursor >= self.logical_capacity {
            return FdRangeScan {
                next: cursor,
                scanned: 0,
                dropped: None,
                done: true,
            };
        }
        let scan_end = end
            .min(self.logical_capacity - 1)
            .min(cursor.saturating_add(scan_budget - 1));
        if let Some(fd) = self.find_next_installed(cursor, scan_end + 1) {
            let dropped = self.drop_fd(fd as i32).ok();
            return FdRangeScan {
                next: fd + 1,
                scanned: fd + 1 - cursor,
                dropped,
                done: fd >= end,
            };
        }
        let next = scan_end + 1;
        FdRangeScan {
            next,
            scanned: next - cursor,
            dropped: None,
            done: next > end || next >= self.logical_capacity,
        }
    }

    fn set_cloexec_range(&mut self, first: u32, last: u32) {
        let Some(end) = self.close_range_end(last) else {
            return;
        };
        let mut range_start = first as usize;
        if range_start > end {
            return;
        }

        if range_start < INLINE_FDS {
            let inline_end = (end + 1).min(INLINE_FDS);
            let lower = usize::MAX << range_start;
            let upper = if inline_end == INLINE_FDS {
                usize::MAX
            } else {
                (1usize << inline_end) - 1
            };
            self.inline_cloexec |= self.inline_open & lower & upper;
            range_start = INLINE_FDS;
        }

        let Some(paged) = self.paged.as_mut().filter(|_| range_start <= end) else {
            self.bump_generation();
            return;
        };
        let first_group = Self::indexes(range_start).0;
        let last_group = Self::indexes(end).0;
        let mut group_cursor = first_group;
        while let Some(group_index) =
            find_set_in_words(&paged.present_groups.data, group_cursor, last_group + 1)
        {
            let group_base = INLINE_FDS + group_index * GROUP_SPAN;
            let group_last = end.min(group_base + GROUP_SPAN - 1);
            let group_first = range_start.max(group_base);
            let first_directory = Self::indexes(group_first).1;
            let last_directory = Self::indexes(group_last).1;
            let group = paged.groups[group_index]
                .as_deref_mut()
                .expect("present group summary must match storage");
            let mut directory_cursor = first_directory;
            while let Some(directory_index) = find_set_in_words(
                &group.present_directories.data,
                directory_cursor,
                last_directory + 1,
            ) {
                let directory_span = INDEX_FANOUT * CHUNK_FDS;
                let directory_base = group_base + directory_index * directory_span;
                let directory_last = group_last.min(directory_base + directory_span - 1);
                let directory_first = group_first.max(directory_base);
                let first_chunk = Self::indexes(directory_first).2;
                let last_chunk = Self::indexes(directory_last).2;
                let directory = group.directories[directory_index]
                    .as_deref_mut()
                    .expect("present directory summary must match storage");
                let mut chunk_cursor = first_chunk;
                while let Some(chunk_index) =
                    find_set_in_words(&directory.present_chunks.data, chunk_cursor, last_chunk + 1)
                {
                    let chunk_base = directory_base + chunk_index * CHUNK_FDS;
                    let chunk_first = directory_first.max(chunk_base) - chunk_base;
                    let chunk_end = (directory_last + 1).min(chunk_base + CHUNK_FDS) - chunk_base;
                    let chunk = directory.chunks[chunk_index]
                        .as_deref_mut()
                        .expect("present chunk summary must match storage");
                    let first_word = chunk_first / BITS_PER_WORD;
                    let last_word = (chunk_end - 1) / BITS_PER_WORD;
                    for word_index in first_word..=last_word {
                        let word_base = word_index * BITS_PER_WORD;
                        let lower = chunk_first.saturating_sub(word_base).min(BITS_PER_WORD);
                        let upper = chunk_end.saturating_sub(word_base).min(BITS_PER_WORD);
                        let lower_mask = usize::MAX << lower;
                        let upper_mask = if upper == BITS_PER_WORD {
                            usize::MAX
                        } else {
                            (1usize << upper) - 1
                        };
                        chunk.close_on_exec.data[word_index] |=
                            chunk.open_fds.data[word_index] & lower_mask & upper_mask;
                    }
                    chunk_cursor = chunk_index + 1;
                }
                directory_cursor = directory_index + 1;
            }
            group_cursor = group_index + 1;
        }
        self.bump_generation();
    }

    fn set_cloexec(&mut self, fd: i32, value: bool) -> Result<(), SystemError> {
        if fd < 0 || self.file_ref(fd as usize).is_none() {
            return Err(SystemError::EBADF);
        }
        self.set_cloexec_raw(fd as usize, value);
        self.bump_generation();
        Ok(())
    }

    fn take_next_cloexec(&mut self, start: usize) -> Option<(usize, DroppedFd)> {
        let mut cursor = start;
        while let Some(fd) = self.find_next_installed(cursor, self.logical_capacity) {
            if self.get_cloexec_raw(fd) {
                let dropped = self.drop_fd(fd as i32).ok()?;
                return Some((fd + 1, dropped));
            }
            cursor = fd + 1;
        }
        None
    }

    fn clone_chunk_count(&self, punch_hole: Option<(u32, u32)>) -> usize {
        let mut count = 0;
        let mut last = None;
        let mut cursor = INLINE_FDS;
        while let Some(fd) = self.find_next_installed(cursor, self.logical_capacity) {
            cursor = fd + 1;
            if punch_hole.is_some_and(|(first, end)| fd >= first as usize && fd <= end as usize) {
                continue;
            }
            let chunk = Self::chunk_number(fd);
            if last != Some(chunk) {
                count += 1;
                last = Some(chunk);
            }
        }
        count
    }

    fn collect_clone_chunks(
        &self,
        punch_hole: Option<(u32, u32)>,
        chunk_numbers: &mut Vec<usize>,
    ) -> usize {
        let mut highest = None;
        let mut cursor = 0;
        while let Some(fd) = self.find_next_installed(cursor, self.logical_capacity) {
            cursor = fd + 1;
            if punch_hole.is_some_and(|(first, last)| fd >= first as usize && fd <= last as usize) {
                continue;
            }
            highest = Some(fd);
            if fd >= INLINE_FDS {
                let chunk_number = Self::chunk_number(fd);
                if chunk_numbers.last().copied() != Some(chunk_number) {
                    assert!(chunk_numbers.len() < chunk_numbers.capacity());
                    chunk_numbers.push(chunk_number);
                }
            }
        }
        highest.map_or(INLINE_FDS, |fd| fd + 1)
    }

    fn prepare_clone_layout(
        &mut self,
        plan: &FdClonePlan,
        creator: &Arc<ObjectAccount>,
    ) -> Result<(), MemoryAllocationError> {
        let mut fds = Vec::new();
        fds.try_reserve_exact(plan.chunk_numbers.len())
            .map_err(|_| SystemError::ENOMEM)?;
        for &chunk_number in &plan.chunk_numbers {
            fds.push(Self::chunk_base(chunk_number));
        }
        self.ensure_fds_materialized(&fds, creator)?;
        self.logical_capacity = plan.logical_capacity;
        self.rebuild_summaries();
        Ok(())
    }

    fn populate_clone(&self, target: &mut Self, punch_hole: Option<(u32, u32)>) {
        let mut cursor = 0;
        while let Some(fd) = self.find_next_installed(cursor, self.logical_capacity) {
            cursor = fd + 1;
            if punch_hole.is_some_and(|(first, last)| fd >= first as usize && fd <= last as usize) {
                continue;
            }
            let file = self.file_ref(fd).unwrap().clone();
            target.set_slot(fd, Some(file), true, self.get_cloexec_raw(fd));
        }
        target.next_fd = target
            .find_next_free(0, target.logical_capacity)
            .unwrap_or(target.logical_capacity);
    }

    pub fn iter(&self) -> FileDescriptorIterator<'_> {
        FileDescriptorIterator {
            state: self,
            next: 0,
        }
    }
}

impl Drop for FdTableState {
    fn drop(&mut self) {
        let mut cursor = 0;
        while let Some(fd) = self.find_next_installed(cursor, self.logical_capacity) {
            cursor = fd + 1;
            if let Ok(dropped) = self.drop_fd(fd as i32) {
                if let Err(error) = dropped.finish_close() {
                    log::warn!("fd table teardown close failed: {:?}", error);
                }
            }
        }
    }
}

struct FdClonePlan {
    generation: u64,
    logical_capacity: usize,
    chunk_numbers: Vec<usize>,
}

#[derive(Debug)]
pub struct FileDescriptorTable {
    body: OwnedBox<FdTableData>,
}

#[derive(Debug)]
pub struct FdTableData {
    inner: RwSem<FdTableState>,
    task_users: AtomicUsize,
}

const _: () = {
    assert!(size_of::<FdTableData>() <= SLOT_BLOCK_BYTES);
};

impl Deref for FileDescriptorTable {
    type Target = FdTableData;
    fn deref(&self) -> &FdTableData {
        &self.body
    }
}

impl FileDescriptorTable {
    pub(crate) fn try_new(
        inner: FdTableState,
        creator: &Arc<ObjectAccount>,
    ) -> Result<Self, MemoryAllocationError> {
        Ok(Self {
            body: OwnedBox::prepare(creator, KernelByteKind::SlabUnreclaimable)?
                .try_init(FdTableData {
                    inner: RwSem::new(inner),
                    task_users: AtomicUsize::new(0),
                })
                .map_err(|_| SystemError::ENOMEM)?,
        })
    }

    fn mutate_prepared<const N: usize, R>(
        &self,
        creator: &Arc<ObjectAccount>,
        select: impl Fn(&FdTableState) -> Result<[usize; N], SystemError>,
        commit: impl FnOnce(&mut FdTableState, [usize; N]) -> Result<R, SystemError>,
    ) -> Result<R, MemoryAllocationError> {
        loop {
            let mut needs = [None; N];
            let snapshot = {
                let state = self.inner.read();
                select(&state).and_then(|fds| {
                    let required = fds
                        .iter()
                        .copied()
                        .max()
                        .and_then(|fd| fd.checked_add(1))
                        .unwrap_or(INLINE_FDS);
                    let capacity = state.planned_capacity(required)?;
                    state.collect_path_needs(&fds, &mut needs);
                    Ok((state.content_generation, fds, capacity))
                })
            };
            let (generation, fds, capacity) = snapshot?;
            let mut paths: [Option<PreparedPath>; N] = core::array::from_fn(|_| None);
            for (slot, need) in paths.iter_mut().zip(needs) {
                if let Some(need) = need {
                    *slot = Some(PreparedPath::try_new(need, creator)?);
                }
            }
            let mut state = self.inner.write();
            if state.content_generation != generation {
                drop(state);
                drop(paths);
                continue;
            }
            state.commit_paths(&mut paths);
            if state.logical_capacity != capacity {
                state.logical_capacity = capacity;
                state.rebuild_summaries();
            }
            let result = commit(&mut state, fds);
            drop(state);
            drop(paths); // canceled candidates never retire under the table guard.
            return result.map_err(Into::into);
        }
    }

    /// Read-only compatibility view. Mutable guards are intentionally not
    /// exposed; every mutation must pass through a table transaction method.
    pub fn read(&self) -> RwSemReadGuard<'_, FdTableState> {
        self.inner.read()
    }

    pub(crate) fn attach_task(&self) {
        self.task_users
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |users| {
                users.checked_add(1)
            })
            .expect("fd-table task-user count overflow");
    }

    pub(crate) fn detach_task(&self) {
        self.task_users
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |users| {
                users.checked_sub(1)
            })
            .expect("fd-table task-user count underflow");
    }

    pub(crate) fn is_shared_by_tasks(&self) -> bool {
        self.task_user_count() > 1
    }

    pub(crate) fn task_user_count(&self) -> usize {
        self.task_users.load(Ordering::Acquire)
    }

    pub fn get_file_by_fd(&self, fd: i32) -> Option<Arc<File>> {
        self.inner.read().get_file_by_fd(fd)
    }

    pub fn get_pair(&self, first: i32, second: i32) -> Option<(Arc<File>, Arc<File>)> {
        let state = self.inner.read();
        Some((state.get_file_by_fd(first)?, state.get_file_by_fd(second)?))
    }

    pub fn alloc_fd(
        &self,
        file: File,
        cloexec: bool,
        soft_limit: usize,
        creator: &Arc<ObjectAccount>,
    ) -> Result<i32, MemoryAllocationError> {
        let file = Arc::try_new(file).map_err(|_| SystemError::ENOMEM)?;
        self.alloc_fd_arc(file, cloexec, soft_limit, creator)
    }

    pub fn alloc_fd_arc(
        &self,
        file: Arc<File>,
        cloexec: bool,
        soft_limit: usize,
        creator: &Arc<ObjectAccount>,
    ) -> Result<i32, MemoryAllocationError> {
        // Keep an outer pin even when validation/commit returns an error.
        self.mutate_prepared(
            creator,
            |state| {
                state
                    .select_reservation::<1>(soft_limit, 0, cloexec)
                    .map(|fds| fds.map(|fd| fd as usize))
            },
            |state, [fd]| {
                state.commit_reservation([fd as i32], cloexec);
                state.install_reserved(fd, file.clone())?;
                Ok(fd as i32)
            },
        )
    }

    /// Duplicate `oldfd` into the lowest free slot.
    pub fn duplicate(
        &self,
        oldfd: i32,
        cloexec: bool,
        soft_limit: usize,
        creator: &Arc<ObjectAccount>,
    ) -> Result<i32, MemoryAllocationError> {
        self.mutate_prepared(
            creator,
            |state| {
                if oldfd < 0 || state.file_ref(oldfd as usize).is_none() {
                    return Err(SystemError::EBADF);
                }
                state
                    .select_reservation::<1>(soft_limit, 0, cloexec)
                    .map(|fds| fds.map(|fd| fd as usize))
            },
            |state, [fd]| {
                let file = state
                    .get_file_by_fd(oldfd)
                    .expect("generation-validated source");
                state.commit_reservation([fd as i32], cloexec);
                state.install_reserved(fd, file)?;
                Ok(fd as i32)
            },
        )
    }

    /// Duplicate `oldfd` into the first free slot at or above `min_fd`.
    ///
    /// Looking up the source and reserving/installing the destination share one
    /// write-side transaction, matching Linux's files-lock serialization with
    /// concurrent close/dup operations.
    pub fn duplicate_min(
        &self,
        oldfd: i32,
        min_fd: i32,
        cloexec: bool,
        soft_limit: usize,
        creator: &Arc<ObjectAccount>,
    ) -> Result<i32, MemoryAllocationError> {
        self.mutate_prepared(
            creator,
            |state| {
                if state.file_ref(oldfd as usize).is_none() || oldfd < 0 {
                    return Err(SystemError::EBADF);
                }
                if min_fd < 0 || min_fd as usize >= soft_limit {
                    return Err(SystemError::EINVAL);
                }
                state
                    .select_reservation::<1>(soft_limit, min_fd as usize, cloexec)
                    .map(|fds| fds.map(|fd| fd as usize))
            },
            |state, [fd]| {
                let file = state
                    .get_file_by_fd(oldfd)
                    .expect("generation-validated source");
                state.commit_reservation([fd as i32], cloexec);
                state.install_reserved(fd, file)?;
                Ok(fd as i32)
            },
        )
    }

    pub fn reserve<const N: usize>(
        self: &Arc<Self>,
        soft_limit: usize,
        min_fd: usize,
        cloexec: bool,
        creator: &Arc<ObjectAccount>,
    ) -> Result<FdReservation<N>, MemoryAllocationError> {
        let fds = self.mutate_prepared(
            creator,
            |state| {
                state
                    .select_reservation::<N>(soft_limit, min_fd, cloexec)
                    .map(|fds| fds.map(|fd| fd as usize))
            },
            |state, fds| {
                let fds = fds.map(|fd| fd as i32);
                state.commit_reservation(fds, cloexec);
                Ok(fds)
            },
        )?;
        Ok(FdReservation {
            table: self.clone(),
            fds,
            active: true,
        })
    }

    pub fn drop_fd(&self, fd: i32) -> Result<DroppedFd, SystemError> {
        self.inner.write().drop_fd(fd)
    }

    pub fn duplicate_exact(
        &self,
        oldfd: i32,
        newfd: i32,
        cloexec: bool,
        soft_limit: usize,
        creator: &Arc<ObjectAccount>,
    ) -> Result<(i32, Option<DroppedFd>), MemoryAllocationError> {
        let result = self.mutate_prepared(
            creator,
            |state| {
                if oldfd < 0 || state.file_ref(oldfd as usize).is_none() {
                    return Err(SystemError::EBADF);
                }
                if newfd < 0 || newfd as usize >= soft_limit {
                    return Err(SystemError::EBADF);
                }
                Ok([newfd as usize])
            },
            |state, [_]| {
                let file = state
                    .get_file_by_fd(oldfd)
                    .expect("generation-validated source");
                state.replace_arc(file, newfd, cloexec, soft_limit)
            },
        );
        result.map_err(|error| match error {
            MemoryAllocationError::System(SystemError::EMFILE) => SystemError::EBADF.into(),
            error => error,
        })
    }

    pub(crate) fn close_range_end(&self, last: u32) -> Option<usize> {
        self.inner.read().close_range_end(last)
    }

    pub(crate) fn take_next_open_in_range(
        &self,
        cursor: usize,
        end: usize,
        scan_budget: usize,
    ) -> FdRangeScan {
        self.inner
            .write()
            .take_next_open_in_range(cursor, end, scan_budget)
    }

    pub(crate) fn set_cloexec_range(&self, first: u32, last: u32) {
        self.inner.write().set_cloexec_range(first, last);
    }

    pub fn set_cloexec(&self, fd: i32, value: bool) -> Result<(), SystemError> {
        self.inner.write().set_cloexec(fd, value)
    }

    pub fn take_next_cloexec(&self, start: usize) -> Option<(usize, DroppedFd)> {
        self.inner.write().take_next_cloexec(start)
    }

    pub(crate) fn try_clone(
        source: &Arc<Self>,
        punch_hole: Option<(u32, u32)>,
        creator: &Arc<ObjectAccount>,
    ) -> Result<Arc<Self>, MemoryAllocationError> {
        loop {
            let (generation, chunk_count) = {
                let guard = source.inner.read();
                (
                    guard.content_generation,
                    guard.clone_chunk_count(punch_hole),
                )
            };
            let mut chunks = Vec::new();
            chunks
                .try_reserve_exact(chunk_count)
                .map_err(|_| SystemError::ENOMEM)?;
            let required = {
                let guard = source.inner.read();
                if guard.content_generation != generation {
                    drop(guard);
                    drop(chunks);
                    continue;
                }
                guard.collect_clone_chunks(punch_hole, &mut chunks)
            };
            let plan = FdClonePlan {
                generation,
                logical_capacity: FdTableState::planned_capacity_from_inline(required)?,
                chunk_numbers: chunks,
            };
            let mut target_state = FdTableState::new();
            target_state.prepare_clone_layout(&plan, creator)?;
            let target = Arc::try_new(Self::try_new(target_state, creator)?)
                .map_err(|_| SystemError::ENOMEM)?;

            let source_guard = source.inner.read();
            if source_guard.content_generation != plan.generation {
                drop(source_guard);
                drop(target);
                continue;
            }
            source_guard.populate_clone(&mut target.inner.write(), punch_hole);
            drop(source_guard);
            return Ok(target);
        }
    }
}

impl Drop for FileDescriptorTable {
    fn drop(&mut self) {
        debug_assert_eq!(self.task_users.load(Ordering::Relaxed), 0);
    }
}

/// Exercise the actual kernel RwSem/body layout, not the host fixture's lock.
/// Reserved inline slots require no File endpoints or paged storage.
pub(crate) fn run_body_layout_selftest() -> Result<(bool, usize, u64), MemoryAllocationError> {
    use crate::cgroup::memory::{object::ObjectAccount, MemoryAccount};
    use crate::mm::allocator::kernel_allocator::allocation_footprint;

    let account = MemoryAccount::new(0x776610, None)?;
    let creator = ObjectAccount::new(&account)?;
    let bytes = size_of::<FdTableData>();
    let footprint = allocation_footprint(core::alloc::Layout::new::<FdTableData>());
    let body_kind = KernelByteKind::SlabUnreclaimable
        .for_allocation(core::alloc::Layout::new::<FdTableData>()) as usize;
    // Production callers use N=1/2; avoid a 64-entry PreparedPath temporary
    // on the kernel stack just to retain all inline reservations in this test.
    let mut reservations = Vec::new();
    reservations
        .try_reserve_exact(INLINE_FDS)
        .map_err(|_| SystemError::ENOMEM)?;
    let table = Arc::try_new(FileDescriptorTable::try_new(FdTableState::new(), &creator)?)
        .map_err(|_| SystemError::ENOMEM)?;
    let weak = Arc::downgrade(&table);
    let baseline = account.snapshot();
    for _ in 0..INLINE_FDS {
        reservations.push(table.reserve::<1>(INLINE_FDS, 0, false, &creator)?);
    }
    let mut ok = bytes <= SLOT_BLOCK_BYTES
        && footprint <= SLOT_BLOCK_BYTES as u64
        && baseline.kernel_bytes[body_kind] == footprint;
    {
        let state = table.read();
        ok &= state.paged.is_none()
            && state.logical_capacity == INLINE_FDS
            && state.inline_open == usize::MAX
            && reservations[0].fd(0) == 0
            && reservations[INLINE_FDS - 1].fd(0) == INLINE_FDS as i32 - 1;
    }
    ok &= account.snapshot().kernel_bytes == baseline.kernel_bytes;
    drop(reservations);
    {
        let state = table.read();
        ok &= state.inline_open == 0 && state.paged.is_none();
    }
    drop(table);
    ok &= weak.upgrade().is_none()
        && account.current_pages() == 0
        && account.snapshot().kernel_bytes
            == [0; crate::cgroup::memory::accounting::KERNEL_BYTE_KIND_COUNT];
    drop(weak);
    Ok((ok, bytes, footprint))
}

#[derive(Debug)]
pub struct FdReservation<const N: usize> {
    table: Arc<FileDescriptorTable>,
    fds: [i32; N],
    active: bool,
}

impl<const N: usize> FdReservation<N> {
    pub fn fd(&self, index: usize) -> i32 {
        self.fds[index]
    }

    fn install_arcs(mut self, files: [Arc<File>; N]) -> Result<[i32; N], SystemError> {
        {
            let mut state = self.table.inner.write();
            for &fd in &self.fds {
                let index = fd as usize;
                if !state.is_open(index) || state.file_ref(index).is_some() {
                    return Err(SystemError::EBADF);
                }
            }
            for (&fd, file) in self.fds.iter().zip(files) {
                state.install_reserved(fd as usize, file)?;
            }
        }
        self.active = false;
        Ok(self.fds)
    }
}

impl FdReservation<1> {
    /// Publish an already allocated file after its reserved number was copied
    /// to userspace. Reservation owns a materialized, uninstalled slot: close
    /// cannot remove it and dup2/dup3 cannot replace it. No allocation or
    /// recoverable failure is possible here.
    pub fn commit_arc(self, file: Arc<File>) -> i32 {
        self.install_arc(file)
            .expect("reserved fd must remain available until commit")
    }

    pub fn install(self, file: File) -> Result<i32, SystemError> {
        let file = Arc::try_new(file).map_err(|_| SystemError::ENOMEM)?;
        self.install_arc(file)
    }

    pub fn install_arc(self, file: Arc<File>) -> Result<i32, SystemError> {
        Ok(self.install_arcs([file])?[0])
    }
}

impl FdReservation<2> {
    pub fn install_arc_pair(
        self,
        first: Arc<File>,
        second: Arc<File>,
    ) -> Result<[i32; 2], SystemError> {
        self.install_arcs([first, second])
    }
}

impl<const N: usize> Drop for FdReservation<N> {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let mut state = self.table.inner.write();
        for &fd in &self.fds {
            state.release_reserved(fd as usize);
        }
        self.active = false;
    }
}

pub struct FileDescriptorIterator<'a> {
    state: &'a FdTableState,
    next: usize,
}

impl Iterator for FileDescriptorIterator<'_> {
    type Item = (i32, Arc<File>);

    fn next(&mut self) -> Option<Self::Item> {
        let fd = self
            .state
            .find_next_installed(self.next, self.state.logical_capacity)?;
        self.next = fd + 1;
        Some((fd as i32, self.state.file_ref(fd)?.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> (
        Arc<FileDescriptorTable>,
        Arc<crate::cgroup::memory::MemoryAccount>,
        Arc<ObjectAccount>,
    ) {
        let root = crate::cgroup::memory::MemoryAccount::new(700, None).unwrap();
        let owner = crate::cgroup::memory::MemoryAccount::new(701, Some(root)).unwrap();
        let creator = ObjectAccount::new(&owner).unwrap();
        let table = Arc::new(FileDescriptorTable::try_new(FdTableState::new(), &creator).unwrap());
        (table, owner, creator)
    }

    #[test]
    fn actual_table_body_and_weak_header_release_budget() {
        let (table, owner, creator) = table();
        let weak = Arc::downgrade(&table);
        assert!(owner.snapshot().current > 0);
        let before = owner.snapshot();
        let reservation = table.reserve::<1>(1024, 0, true, &creator).unwrap();
        assert_eq!(owner.snapshot().current, before.current); // common inline path adds no backing.
        drop(reservation);
        drop(table);
        assert!(weak.upgrade().is_none());
        assert_eq!(owner.snapshot().current, 0);
        assert_eq!(
            owner.snapshot().kernel_bytes,
            [0; crate::cgroup::memory::accounting::KERNEL_BYTE_KIND_COUNT]
        );
        drop(weak);
    }

    #[test]
    fn growth_quota_refusal_preserves_reservation_state() {
        let (table, owner, creator) = table();
        let before = owner.snapshot();
        let original = {
            let state = table.read();
            (
                state.logical_capacity,
                state.next_fd,
                state.content_generation,
            )
        };
        owner.set_policy(crate::cgroup::memory::MemoryPolicy {
            max: Some(before.current),
            ..Default::default()
        });
        assert!(matches!(
            table.reserve::<2>(4096, 3000, true, &creator),
            Err(MemoryAllocationError::Charge(_))
        ));
        let after = owner.snapshot();
        assert_eq!(after.current, before.current);
        assert_eq!(after.kernel_bytes, before.kernel_bytes);
        {
            let state = table.read();
            assert_eq!(
                (
                    state.logical_capacity,
                    state.next_fd,
                    state.content_generation
                ),
                original
            );
            assert!(state.paged.is_none());
            assert_eq!(state.fd_open_count(), 0);
        }
        owner.set_policy(Default::default());
        let reservation = table.reserve::<2>(4096, 3000, true, &creator).unwrap();
        assert_eq!(reservation.fd(0), 3000);
        assert_eq!(reservation.fd(1), 3001);
        assert!(owner.snapshot().current > before.current);
        drop(reservation);
        drop(table);
        assert_eq!(owner.snapshot().current, 0);
    }

    #[test]
    fn highest_integer_descriptor_and_root_block_boundaries_fit() {
        let (group, directory, chunk, slot) = FdTableState::indexes(i32::MAX as usize);
        assert!(group < ROOT_ENTRIES_MAX);
        assert!(directory < INDEX_FANOUT && chunk < INDEX_FANOUT && slot < CHUNK_FDS);
        assert_eq!(size_of::<RootBlock>(), SLOT_BLOCK_BYTES);
        assert!(size_of::<FdDirectory>() <= SLOT_BLOCK_BYTES);
        assert!(size_of::<FdGroup>() <= SLOT_BLOCK_BYTES);
    }

    #[test]
    fn sparse_root_blocks_materialize_only_requested_extremes() {
        struct RestoreNrOpen(usize);
        impl Drop for RestoreNrOpen {
            fn drop(&mut self) {
                NR_OPEN.store(self.0, Ordering::Release);
            }
        }
        let _restore = RestoreNrOpen(NR_OPEN.swap(nr_open_max(), Ordering::AcqRel));
        let (table, owner, creator) = table();
        let last = nr_open_max() - 1;
        let high = table
            .reserve::<1>(nr_open_max(), last, true, &creator)
            .unwrap();
        assert_eq!(high.fd(0) as usize, last);
        let low = table
            .reserve::<1>(nr_open_max(), INLINE_FDS, false, &creator)
            .unwrap();
        {
            let state = table.read();
            let roots = &state.paged.as_ref().unwrap().groups;
            assert!(roots.blocks[0].is_some());
            assert!(roots.blocks[ROOT_BLOCKS - 1].is_some());
            assert!(roots.blocks[1..ROOT_BLOCKS - 1].iter().all(Option::is_none));
            assert!(state.chunk(last).is_some());
            assert!(state.chunk(INLINE_FDS).is_some());
        }
        drop(high);
        drop(low);
        drop(table);
        assert_eq!(owner.snapshot().current, 0);
    }

    #[test]
    fn later_growth_uses_actor_and_offline_reparents_only_old_stock() {
        let root = crate::cgroup::memory::MemoryAccount::new(730, None).unwrap();
        let old = crate::cgroup::memory::MemoryAccount::new(731, Some(root.clone())).unwrap();
        let new = crate::cgroup::memory::MemoryAccount::new(732, Some(root.clone())).unwrap();
        let old_creator = ObjectAccount::new(&old).unwrap();
        let new_creator = ObjectAccount::new(&new).unwrap();
        let table =
            Arc::new(FileDescriptorTable::try_new(FdTableState::new(), &old_creator).unwrap());
        let old_bytes = old.snapshot().kernel_bytes;
        let reservation = table.reserve::<1>(4096, 3000, true, &new_creator).unwrap();
        assert_eq!(old.snapshot().kernel_bytes, old_bytes);
        assert!(new.snapshot().current > 0);
        let new_bytes = new.snapshot().kernel_bytes;
        crate::cgroup::memory::object::offline_memory_account(&old).unwrap();
        assert_eq!(old.snapshot().current, 0);
        assert_eq!(new.snapshot().kernel_bytes, new_bytes);
        assert!(root.snapshot().current > new.snapshot().current);
        drop(reservation);
        drop(table);
        assert_eq!(root.snapshot().current, 0);
        assert_eq!(new.snapshot().current, 0);
        assert_eq!(
            root.snapshot().kernel_bytes,
            [0; crate::cgroup::memory::accounting::KERNEL_BYTE_KIND_COUNT]
        );
    }

    // The historical state-only cases exercise the same private materialize
    // and reservation primitives, without holding a table/resource guard.
    fn reserve<const N: usize>(
        state: &mut FdTableState,
        limit: usize,
        min: usize,
        cloexec: bool,
    ) -> [i32; N] {
        let owner = crate::cgroup::memory::MemoryAccount::new(1, None).unwrap();
        let creator = ObjectAccount::new(&owner).unwrap();
        let fds = state.select_reservation::<N>(limit, min, cloexec).unwrap();
        state
            .ensure_fds_materialized(&fds.map(|fd| fd as usize), &creator)
            .unwrap();
        state.commit_reservation(fds, cloexec);
        fds
    }

    #[test]
    fn sparse_layout_keeps_inline_capacity() {
        let state = FdTableState::new();
        assert_eq!(state.logical_capacity, INLINE_FDS);
        assert!(state.paged.is_none());
    }

    #[test]
    fn index_boundaries_are_stable() {
        assert_eq!(FdTableState::indexes(INLINE_FDS).3, 0);
        assert_eq!(FdTableState::indexes(INLINE_FDS + CHUNK_FDS - 1).3, 511);
        assert_eq!(FdTableState::indexes(INLINE_FDS + CHUNK_FDS).2, 1);
    }

    #[test]
    fn reservation_is_atomic_and_reusable() {
        let mut state = FdTableState::new();
        let fds = reserve::<2>(&mut state, 1024, 0, true);
        assert_eq!(fds, [0, 1]);
        for fd in fds {
            assert!(state.is_open(fd as usize));
            assert!(state.file_ref(fd as usize).is_none());
            assert!(state.get_cloexec(fd));
        }

        state.release_reserved(0);
        state.release_reserved(1);
        assert_eq!(reserve::<1>(&mut state, 1024, 0, false), [0]);
    }

    #[test]
    fn sparse_minimum_materializes_only_its_chunk() {
        let mut state = FdTableState::new();
        let [fd] = reserve::<1>(&mut state, 4096, 3000, false);
        assert_eq!(fd, 3000);
        assert_eq!(state.logical_capacity, 4096);
        assert!(state.chunk(3000).is_some());
        assert!(state.chunk(INLINE_FDS).is_none());
        assert_eq!(state.find_next_free(3000, 4096), Some(3001));
    }

    #[test]
    fn full_inline_summary_advances_to_first_chunk() {
        let mut state = FdTableState::new();
        for expected in 0..INLINE_FDS {
            let [fd] = reserve::<1>(&mut state, 1024, 0, false);
            assert_eq!(fd as usize, expected);
        }
        assert_eq!(state.find_next_free(0, 1024), Some(INLINE_FDS));
        state.release_reserved(17);
        assert_eq!(state.find_next_free(0, 1024), Some(17));
    }

    #[test]
    fn cloexec_range_reaches_sparse_present_chunk() {
        let mut state = FdTableState::new();
        let [fd] = reserve::<1>(&mut state, 600_000, 500_000, false);
        assert!(!state.get_cloexec(fd));
        state.set_cloexec_range(0, u32::MAX);
        assert!(state.get_cloexec(fd));
    }
}
