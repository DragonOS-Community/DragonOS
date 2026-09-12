//! Bounded cache for committed ext4 metadata block images.
//!
//! This module deliberately has no journal or [`super::Ext4`] dependency.  A
//! caller supplies publication and checkpoint events. The mounted filesystem
//! enables it only for a metadata mode whose read, publication, retirement and
//! checkpoint paths are wired as one unit.
#![allow(dead_code)] // Introspection is currently consumed by host-side tests.

use crate::constants::BLOCK_SIZE;
use crate::ext4_defs::{Block, BlockDevice};
use crate::prelude::*;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

const WAYS: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PublicationPoint {
    /// The supplied image is the current home-block view.  This says nothing
    /// about power-loss durability.
    HomeCurrent,
    /// The supplied image is logically visible but has not necessarily been
    /// checkpointed to its home block.
    Accepted { sequence: u64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SlotState {
    Empty,
    Loading { ticket: u64, invalidated: bool },
    Clean,
    Dirty { sequence: u64 },
    Error { code: ErrCode },
}

struct CacheSlot {
    home: Option<PBlockId>,
    image: Vec<u8>,
    state: SlotState,
    referenced: bool,
}

impl CacheSlot {
    fn try_new() -> core::result::Result<Self, ()> {
        // Construct the 4 KiB owner through Vec's fallible reservation.  A
        // plain Box::new([0; BLOCK_SIZE]) would make cache enablement capable
        // of aborting the kernel on allocation failure.
        let mut image = Vec::new();
        image.try_reserve_exact(BLOCK_SIZE).map_err(|_| ())?;
        image.resize(BLOCK_SIZE, 0);
        Ok(Self {
            home: None,
            image,
            state: SlotState::Empty,
            referenced: false,
        })
    }

    fn clear(&mut self) {
        self.home = None;
        self.state = SlotState::Empty;
        self.referenced = false;
    }

    fn reclaimable(&self) -> bool {
        matches!(self.state, SlotState::Clean | SlotState::Error { .. })
    }
}

struct CacheSet {
    ways: [CacheSlot; WAYS],
    hand: usize,
}

impl CacheSet {
    fn try_new() -> core::result::Result<Self, ()> {
        let mut ways = Vec::new();
        ways.try_reserve_exact(WAYS).map_err(|_| ())?;
        for _ in 0..WAYS {
            ways.push(CacheSlot::try_new()?);
        }
        let ways = ways.try_into().map_err(|_| ())?;
        Ok(Self { ways, hand: 0 })
    }

    fn find(&self, home: PBlockId) -> Option<usize> {
        self.ways.iter().position(|slot| slot.home == Some(home))
    }

    /// Select an empty way or perform at most two clock passes. Loading and
    /// dirty ways remain pinned. The bounded scan is important because this is
    /// also called by a transaction which may prevent journal retirement.
    fn admit_way(&mut self) -> Option<(usize, bool)> {
        if let Some(index) = self
            .ways
            .iter()
            .position(|slot| slot.state == SlotState::Empty)
        {
            return Some((index, false));
        }

        for _ in 0..WAYS * 2 {
            let index = self.hand;
            self.hand = (self.hand + 1) % WAYS;
            let slot = &mut self.ways[index];
            if !slot.reclaimable() {
                continue;
            }
            if slot.referenced {
                slot.referenced = false;
                continue;
            }
            return Some((index, true));
        }
        None
    }
}

enum MetadataCacheStorage {
    Disabled,
    Enabled(Vec<spin::Mutex<CacheSet>>),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct MetadataCacheStatsSnapshot {
    pub hits: usize,
    pub misses: usize,
    pub loaders: usize,
    pub loading_contention: usize,
    pub io_errors: usize,
    pub evictions: usize,
    pub bypasses: usize,
    pub published_dirty: usize,
    pub checkpoint_cleaned: usize,
    pub invalidations: usize,
    pub stale_loads: usize,
}

#[derive(Default)]
struct MetadataCacheStats {
    hits: AtomicUsize,
    misses: AtomicUsize,
    loaders: AtomicUsize,
    loading_contention: AtomicUsize,
    io_errors: AtomicUsize,
    evictions: AtomicUsize,
    bypasses: AtomicUsize,
    published_dirty: AtomicUsize,
    checkpoint_cleaned: AtomicUsize,
    invalidations: AtomicUsize,
    stale_loads: AtomicUsize,
}

/// Result of one cache read. `notify_progress` is set when this call retired a
/// Loading owner; the future Ext4 integration must advance its wait generation
/// and wake waiters after receiving the outcome.
pub(super) struct MetadataCacheReadOutcome {
    pub result: Result<Block>,
    pub notify_progress: bool,
}

impl MetadataCacheReadOutcome {
    fn ready(block: Block, notify_progress: bool) -> Self {
        Self {
            result: Ok(block),
            notify_progress,
        }
    }

    fn error(code: ErrCode, notify_progress: bool) -> Self {
        Self {
            result: Err(Ext4Error::new(code)),
            notify_progress,
        }
    }
}

/// Fixed-capacity four-way set-associative metadata block cache.
pub(super) struct MetadataBlockCache {
    storage: MetadataCacheStorage,
    next_ticket: AtomicU64,
    stats: MetadataCacheStats,
}

impl MetadataBlockCache {
    /// Build a cache whose usable capacity is rounded down to a multiple of
    /// four. Too-small, overflowing, or failed allocations select Disabled;
    /// cache availability must never decide whether an ext4 mount succeeds.
    pub(super) fn new(requested_blocks: usize) -> Self {
        let set_count = requested_blocks / WAYS;
        let storage = Self::try_storage(set_count).unwrap_or(MetadataCacheStorage::Disabled);
        Self {
            storage,
            // Zero is reserved so a zero-filled/corrupted token cannot match.
            next_ticket: AtomicU64::new(1),
            stats: MetadataCacheStats::default(),
        }
    }

    fn try_storage(set_count: usize) -> core::result::Result<MetadataCacheStorage, ()> {
        if set_count == 0 || set_count.checked_mul(WAYS).is_none() {
            return Err(());
        }
        let mut sets = Vec::new();
        sets.try_reserve_exact(set_count).map_err(|_| ())?;
        for _ in 0..set_count {
            sets.push(spin::Mutex::new(CacheSet::try_new()?));
        }
        Ok(MetadataCacheStorage::Enabled(sets))
    }

    pub(super) fn enabled(&self) -> bool {
        matches!(&self.storage, MetadataCacheStorage::Enabled(_))
    }

    pub(super) fn capacity(&self) -> usize {
        match &self.storage {
            MetadataCacheStorage::Disabled => 0,
            MetadataCacheStorage::Enabled(sets) => sets.len() * WAYS,
        }
    }

    fn set_for(&self, home: PBlockId) -> Option<&spin::Mutex<CacheSet>> {
        let MetadataCacheStorage::Enabled(sets) = &self.storage else {
            return None;
        };
        // Fibonacci hashing prevents aligned ext4 metadata homes from mapping
        // directly by their low bits. Modulo supports non-power-of-two sizes.
        let mixed = home.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        sets.get((mixed % sets.len() as u64) as usize)
    }

    fn allocate_ticket(&self) -> Option<u64> {
        self.next_ticket
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |ticket| {
                ticket.checked_add(1)
            })
            .ok()
            .filter(|ticket| *ticket != 0)
    }

    /// Read one home block. A resident Loading entry returns EAGAIN; admission
    /// failure performs an uncached read so an active transaction never waits
    /// for the checkpoint which it is itself preventing.
    pub(super) fn read(
        &self,
        device: &dyn BlockDevice,
        home: PBlockId,
    ) -> MetadataCacheReadOutcome {
        let Some(set_lock) = self.set_for(home) else {
            self.stats.bypasses.fetch_add(1, Ordering::Relaxed);
            return Self::read_uncached(device, home);
        };

        // Block's owned return image is not part of cache capacity. Allocate it
        // before taking the set lock so allocator work never occurs in a cache
        // critical section.
        let mut hit_image = Box::new([0; BLOCK_SIZE]);
        let ticket = {
            let mut set = set_lock.lock();
            if let Some(index) = set.find(home) {
                let slot = &mut set.ways[index];
                match slot.state {
                    SlotState::Clean | SlotState::Dirty { .. } => {
                        slot.referenced = true;
                        hit_image.copy_from_slice(slot.image.as_ref());
                        self.stats.hits.fetch_add(1, Ordering::Relaxed);
                        return MetadataCacheReadOutcome::ready(Block::new(home, hit_image), false);
                    }
                    SlotState::Loading { .. } => {
                        self.stats
                            .loading_contention
                            .fetch_add(1, Ordering::Relaxed);
                        return MetadataCacheReadOutcome::error(ErrCode::EAGAIN, false);
                    }
                    SlotState::Error { .. } => {
                        let Some(ticket) = self.allocate_ticket() else {
                            self.stats.bypasses.fetch_add(1, Ordering::Relaxed);
                            drop(set);
                            return Self::read_uncached(device, home);
                        };
                        slot.state = SlotState::Loading {
                            ticket,
                            invalidated: false,
                        };
                        slot.referenced = true;
                        ticket
                    }
                    SlotState::Empty => unreachable!("an indexed cache slot cannot be empty"),
                }
            } else {
                self.stats.misses.fetch_add(1, Ordering::Relaxed);
                let Some(ticket) = self.allocate_ticket() else {
                    self.stats.bypasses.fetch_add(1, Ordering::Relaxed);
                    drop(set);
                    return Self::read_uncached(device, home);
                };
                let Some((index, evicted)) = set.admit_way() else {
                    self.stats.bypasses.fetch_add(1, Ordering::Relaxed);
                    drop(set);
                    return Self::read_uncached(device, home);
                };
                if evicted {
                    self.stats.evictions.fetch_add(1, Ordering::Relaxed);
                }
                let slot = &mut set.ways[index];
                slot.home = Some(home);
                slot.state = SlotState::Loading {
                    ticket,
                    invalidated: false,
                };
                slot.referenced = true;
                ticket
            }
        };

        self.stats.loaders.fetch_add(1, Ordering::Relaxed);
        let disk = device.read_block(home);
        let mut set = set_lock.lock();
        let Some(index) = set.find(home) else {
            self.stats.stale_loads.fetch_add(1, Ordering::Relaxed);
            return MetadataCacheReadOutcome::error(ErrCode::EAGAIN, false);
        };
        let slot = &mut set.ways[index];
        match slot.state {
            SlotState::Loading {
                ticket: current,
                invalidated: true,
            } if current == ticket => {
                slot.clear();
                self.stats.stale_loads.fetch_add(1, Ordering::Relaxed);
                MetadataCacheReadOutcome::error(ErrCode::EAGAIN, true)
            }
            SlotState::Loading {
                ticket: current,
                invalidated: false,
            } if current == ticket => match disk {
                Ok(block) => {
                    slot.image.copy_from_slice(block.data.as_ref());
                    slot.state = SlotState::Clean;
                    slot.referenced = true;
                    MetadataCacheReadOutcome::ready(block, true)
                }
                Err(error) => {
                    slot.state = SlotState::Error { code: error.code() };
                    slot.referenced = false;
                    self.stats.io_errors.fetch_add(1, Ordering::Relaxed);
                    MetadataCacheReadOutcome {
                        result: Err(error),
                        notify_progress: true,
                    }
                }
            },
            _ => {
                // Publication won the race and now owns the cache image.
                self.stats.stale_loads.fetch_add(1, Ordering::Relaxed);
                MetadataCacheReadOutcome::error(ErrCode::EAGAIN, false)
            }
        }
    }

    fn read_uncached(device: &dyn BlockDevice, home: PBlockId) -> MetadataCacheReadOutcome {
        MetadataCacheReadOutcome {
            result: device.read_block(home),
            notify_progress: false,
        }
    }

    /// Publish one image if the home is already resident. The return value asks
    /// the caller to notify waiters because a Loading owner was superseded.
    pub(super) fn publish_existing(
        &self,
        home: PBlockId,
        image: &[u8; BLOCK_SIZE],
        point: PublicationPoint,
    ) -> bool {
        let Some(set_lock) = self.set_for(home) else {
            return false;
        };
        let mut set = set_lock.lock();
        let Some(index) = set.find(home) else {
            return false;
        };
        let slot = &mut set.ways[index];
        let ended_loading = matches!(slot.state, SlotState::Loading { .. });
        if let (
            SlotState::Dirty { sequence: current },
            PublicationPoint::Accepted { sequence: incoming },
        ) = (slot.state, point)
        {
            if incoming < current {
                return false;
            }
        }
        slot.image.copy_from_slice(image);
        slot.state = match point {
            PublicationPoint::HomeCurrent => SlotState::Clean,
            PublicationPoint::Accepted { sequence } => {
                self.stats.published_dirty.fetch_add(1, Ordering::Relaxed);
                SlotState::Dirty { sequence }
            }
        };
        slot.referenced = true;
        ended_loading
    }

    /// Mark a resident accepted image clean only when this checkpoint cannot
    /// be older than the cached image.
    pub(super) fn checkpoint(&self, sequence: u64, homes: &[PBlockId]) -> usize {
        let mut cleaned = 0;
        for home in homes.iter().copied() {
            let Some(set_lock) = self.set_for(home) else {
                continue;
            };
            let mut set = set_lock.lock();
            let Some(index) = set.find(home) else {
                continue;
            };
            let slot = &mut set.ways[index];
            if matches!(slot.state, SlotState::Dirty { sequence: dirty } if dirty <= sequence) {
                slot.state = SlotState::Clean;
                slot.referenced = true;
                cleaned += 1;
            }
        }
        self.stats
            .checkpoint_cleaned
            .fetch_add(cleaned, Ordering::Relaxed);
        cleaned
    }

    /// Invalidate one physical home. A Loading slot remains reserved until its
    /// exact loader retires, preventing a train of duplicate loaders.
    pub(super) fn invalidate(&self, home: PBlockId) -> bool {
        let Some(set_lock) = self.set_for(home) else {
            return false;
        };
        let mut set = set_lock.lock();
        let Some(index) = set.find(home) else {
            return false;
        };
        let slot = &mut set.ways[index];
        let notify = match &mut slot.state {
            SlotState::Loading { invalidated, .. } => {
                *invalidated = true;
                true
            }
            _ => {
                slot.clear();
                false
            }
        };
        self.stats.invalidations.fetch_add(1, Ordering::Relaxed);
        notify
    }

    pub(super) fn invalidate_block_ranges<I>(&self, ranges: I) -> bool
    where
        I: Iterator<Item = (PBlockId, PBlockId)> + Clone,
    {
        let MetadataCacheStorage::Enabled(sets) = &self.storage else {
            return false;
        };
        let mut notify = false;
        for set_lock in sets {
            let mut set = set_lock.lock();
            for slot in &mut set.ways {
                let Some(home) = slot.home else {
                    continue;
                };
                if !ranges
                    .clone()
                    .any(|(start, end)| start < end && start <= home && home < end)
                {
                    continue;
                }
                match &mut slot.state {
                    SlotState::Loading { invalidated, .. } => {
                        *invalidated = true;
                        notify = true;
                    }
                    _ => slot.clear(),
                }
                self.stats.invalidations.fetch_add(1, Ordering::Relaxed);
            }
        }
        notify
    }

    /// Reclaim at most `target` clean/error entries without touching Loading or
    /// dirty journal images.
    pub(super) fn reclaim_clean(&self, target: usize) -> usize {
        let MetadataCacheStorage::Enabled(sets) = &self.storage else {
            return 0;
        };
        let mut reclaimed = 0;
        for set_lock in sets {
            if reclaimed == target {
                break;
            }
            let mut set = set_lock.lock();
            for _ in 0..WAYS * 2 {
                if reclaimed == target {
                    break;
                }
                let index = set.hand;
                set.hand = (set.hand + 1) % WAYS;
                let slot = &mut set.ways[index];
                if !slot.reclaimable() {
                    continue;
                }
                if slot.referenced {
                    slot.referenced = false;
                    continue;
                }
                slot.clear();
                reclaimed += 1;
            }
        }
        self.stats.evictions.fetch_add(reclaimed, Ordering::Relaxed);
        reclaimed
    }

    pub(super) fn stats(&self) -> MetadataCacheStatsSnapshot {
        MetadataCacheStatsSnapshot {
            hits: self.stats.hits.load(Ordering::Relaxed),
            misses: self.stats.misses.load(Ordering::Relaxed),
            loaders: self.stats.loaders.load(Ordering::Relaxed),
            loading_contention: self.stats.loading_contention.load(Ordering::Relaxed),
            io_errors: self.stats.io_errors.load(Ordering::Relaxed),
            evictions: self.stats.evictions.load(Ordering::Relaxed),
            bypasses: self.stats.bypasses.load(Ordering::Relaxed),
            published_dirty: self.stats.published_dirty.load(Ordering::Relaxed),
            checkpoint_cleaned: self.stats.checkpoint_cleaned.load(Ordering::Relaxed),
            invalidations: self.stats.invalidations.load(Ordering::Relaxed),
            stale_loads: self.stats.stale_loads.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc as StdArc, Barrier, Condvar, Mutex as StdMutex};
    use std::thread;

    struct TestDevice {
        reads: AtomicUsize,
        fail: AtomicBool,
        gate: Option<StdArc<(StdMutex<bool>, Condvar)>>,
    }

    impl TestDevice {
        fn plain() -> Self {
            Self {
                reads: AtomicUsize::new(0),
                fail: AtomicBool::new(false),
                gate: None,
            }
        }

        fn blocking(gate: StdArc<(StdMutex<bool>, Condvar)>) -> Self {
            Self {
                reads: AtomicUsize::new(0),
                fail: AtomicBool::new(false),
                gate: Some(gate),
            }
        }
    }

    impl BlockDevice for TestDevice {
        fn read_block(&self, block_id: PBlockId) -> Result<Block> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            if let Some(gate) = &self.gate {
                let (lock, wake) = &**gate;
                let mut open = lock.lock().unwrap();
                while !*open {
                    open = wake.wait(open).unwrap();
                }
            }
            if self.fail.load(Ordering::SeqCst) {
                return Err(Ext4Error::new(ErrCode::EIO));
            }
            Ok(Block::new(block_id, Box::new([block_id as u8; BLOCK_SIZE])))
        }

        fn write_block(&self, _block: &Block) -> Result<()> {
            Ok(())
        }

        fn flush(&self) -> Result<()> {
            Ok(())
        }

        fn supports_reliable_flush(&self) -> bool {
            true
        }
    }

    #[test]
    fn too_small_capacity_disables_without_affecting_reads() {
        let cache = MetadataBlockCache::new(WAYS - 1);
        let device = TestDevice::plain();
        assert!(!cache.enabled());
        assert_eq!(cache.capacity(), 0);
        assert_eq!(cache.read(&device, 7).result.unwrap().data[0], 7);
        assert_eq!(cache.stats().bypasses, 1);
    }

    #[test]
    fn impossible_preallocation_falls_back_to_disabled() {
        // This reaches Vec's fallible capacity check without asking the host
        // allocator to commit an enormous mapping.
        let cache = MetadataBlockCache::new(usize::MAX);
        assert!(!cache.enabled());
        assert_eq!(cache.capacity(), 0);
    }

    #[test]
    fn concurrent_cold_read_is_single_flight() {
        let cache = StdArc::new(MetadataBlockCache::new(WAYS));
        let gate = StdArc::new((StdMutex::new(false), Condvar::new()));
        let device = StdArc::new(TestDevice::blocking(StdArc::clone(&gate)));
        let started = StdArc::new(Barrier::new(2));
        let loader_cache = StdArc::clone(&cache);
        let loader_device = StdArc::clone(&device);
        let loader_started = StdArc::clone(&started);
        let loader = thread::spawn(move || {
            loader_started.wait();
            loader_cache.read(loader_device.as_ref(), 11)
        });
        started.wait();
        while device.reads.load(Ordering::SeqCst) == 0 {
            thread::yield_now();
        }
        let contender = cache.read(device.as_ref(), 11);
        assert_eq!(contender.result.unwrap_err().code(), ErrCode::EAGAIN);
        assert!(!contender.notify_progress);
        {
            let (lock, wake) = &*gate;
            *lock.lock().unwrap() = true;
            wake.notify_all();
        }
        let loaded = loader.join().unwrap();
        assert!(loaded.result.is_ok());
        assert!(loaded.notify_progress);
        assert_eq!(device.reads.load(Ordering::SeqCst), 1);
        assert_eq!(cache.read(device.as_ref(), 11).result.unwrap().data[0], 11);
    }

    #[test]
    fn io_error_is_observable_and_next_access_retries() {
        let cache = MetadataBlockCache::new(WAYS);
        let device = TestDevice::plain();
        device.fail.store(true, Ordering::SeqCst);
        let failed = cache.read(&device, 5);
        assert_eq!(failed.result.unwrap_err().code(), ErrCode::EIO);
        assert!(failed.notify_progress);
        device.fail.store(false, Ordering::SeqCst);
        assert!(cache.read(&device, 5).result.is_ok());
        assert_eq!(device.reads.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn publication_wins_over_late_loader() {
        let cache = StdArc::new(MetadataBlockCache::new(WAYS));
        let gate = StdArc::new((StdMutex::new(false), Condvar::new()));
        let device = StdArc::new(TestDevice::blocking(StdArc::clone(&gate)));
        let loader_cache = StdArc::clone(&cache);
        let loader_device = StdArc::clone(&device);
        let loader = thread::spawn(move || loader_cache.read(loader_device.as_ref(), 9));
        while device.reads.load(Ordering::SeqCst) == 0 {
            thread::yield_now();
        }
        let image = [0x5a; BLOCK_SIZE];
        assert!(cache.publish_existing(9, &image, PublicationPoint::Accepted { sequence: 4 }));
        {
            let (lock, wake) = &*gate;
            *lock.lock().unwrap() = true;
            wake.notify_all();
        }
        let stale = loader.join().unwrap();
        assert_eq!(stale.result.unwrap_err().code(), ErrCode::EAGAIN);
        assert_eq!(cache.read(device.as_ref(), 9).result.unwrap().data[0], 0x5a);
    }

    #[test]
    fn old_checkpoint_does_not_clean_newer_publication() {
        let cache = MetadataBlockCache::new(WAYS);
        let device = TestDevice::plain();
        cache.read(&device, 3).result.unwrap();
        assert!(
            cache.publish_existing(
                3,
                &[10; BLOCK_SIZE],
                PublicationPoint::Accepted { sequence: 10 }
            ) == false
        );
        cache.publish_existing(
            3,
            &[11; BLOCK_SIZE],
            PublicationPoint::Accepted { sequence: 11 },
        );
        assert!(!cache.publish_existing(
            3,
            &[10; BLOCK_SIZE],
            PublicationPoint::Accepted { sequence: 10 },
        ));
        assert_eq!(cache.checkpoint(10, &[3]), 0);
        // Dirty entries are pinned, so neither reclaim nor the old checkpoint
        // may discard the current accepted image.
        assert_eq!(cache.reclaim_clean(1), 0);
        assert_eq!(cache.read(&device, 3).result.unwrap().data[0], 11);
        assert_eq!(cache.checkpoint(11, &[3]), 1);
        assert_eq!(cache.reclaim_clean(1), 1);
    }

    #[test]
    fn invalidated_loader_holds_way_until_io_retires() {
        let cache = StdArc::new(MetadataBlockCache::new(WAYS));
        let gate = StdArc::new((StdMutex::new(false), Condvar::new()));
        let device = StdArc::new(TestDevice::blocking(StdArc::clone(&gate)));
        let loader_cache = StdArc::clone(&cache);
        let loader_device = StdArc::clone(&device);
        let loader = thread::spawn(move || loader_cache.read(loader_device.as_ref(), 13));
        while device.reads.load(Ordering::SeqCst) == 0 {
            thread::yield_now();
        }
        assert!(cache.invalidate(13));
        assert_eq!(
            cache.read(device.as_ref(), 13).result.unwrap_err().code(),
            ErrCode::EAGAIN
        );
        {
            let (lock, wake) = &*gate;
            *lock.lock().unwrap() = true;
            wake.notify_all();
        }
        let retired = loader.join().unwrap();
        assert_eq!(retired.result.unwrap_err().code(), ErrCode::EAGAIN);
        assert!(retired.notify_progress);
        assert!(cache.read(device.as_ref(), 13).result.is_ok());
        assert_eq!(device.reads.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn retirement_ranges_invalidate_only_matching_homes() {
        let cache = MetadataBlockCache::new(WAYS * 4);
        let device = TestDevice::plain();
        for home in [4, 5, 20] {
            cache.read(&device, home).result.unwrap();
        }
        assert!(!cache.invalidate_block_ranges([(4, 6)].into_iter()));
        let before = device.reads.load(Ordering::SeqCst);
        cache.read(&device, 4).result.unwrap();
        cache.read(&device, 5).result.unwrap();
        cache.read(&device, 20).result.unwrap();
        assert_eq!(device.reads.load(Ordering::SeqCst) - before, 2);
    }

    #[test]
    fn pinned_set_bypasses_without_eviction_or_wait() {
        let cache = MetadataBlockCache::new(WAYS);
        let device = TestDevice::plain();
        for home in 0..WAYS as u64 {
            cache.read(&device, home).result.unwrap();
            cache.publish_existing(
                home,
                &[home as u8; BLOCK_SIZE],
                PublicationPoint::Accepted { sequence: home + 1 },
            );
        }
        let reads = device.reads.load(Ordering::SeqCst);
        assert!(cache.read(&device, 99).result.is_ok());
        assert_eq!(device.reads.load(Ordering::SeqCst), reads + 1);
        assert_eq!(cache.stats().bypasses, 1);
    }

    #[test]
    fn home_current_publication_is_clean_and_reclaimable() {
        let cache = MetadataBlockCache::new(WAYS);
        let device = TestDevice::plain();
        cache.read(&device, 1).result.unwrap();
        cache.publish_existing(1, &[8; BLOCK_SIZE], PublicationPoint::HomeCurrent);
        assert_eq!(cache.read(&device, 1).result.unwrap().data[0], 8);
        assert_eq!(cache.reclaim_clean(1), 1);
    }
}
