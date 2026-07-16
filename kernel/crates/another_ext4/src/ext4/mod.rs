use crate::constants::*;
use crate::ext4_defs::*;
use crate::prelude::*;
use crate::return_error;
use core::sync::atomic::{AtomicUsize, Ordering};

mod alloc;
mod dir;
mod extent;
mod high_level;
mod journal;
mod journal_recovery;
mod journal_transaction;
mod link;
mod low_level;
mod orphan;
mod rw;
mod xattr_reclaim;

pub use low_level::SetAttr;

/// Simple fixed-size inode cache.
/// When full, the entire cache is cleared (simple but effective for common workloads).
struct InodeCache {
    entries: BTreeMap<InodeId, InodeRef>,
    max_size: usize,
}

impl InodeCache {
    fn new(max_size: usize) -> Self {
        Self {
            entries: BTreeMap::new(),
            max_size,
        }
    }

    fn get(&self, id: InodeId) -> Option<InodeRef> {
        self.entries.get(&id).cloned()
    }

    fn insert(&mut self, inode_ref: InodeRef) {
        if self.entries.len() >= self.max_size {
            // Simple eviction: clear all when full
            self.entries.clear();
        }
        self.entries.insert(inode_ref.id, inode_ref);
    }

    fn invalidate(&mut self, id: InodeId) {
        self.entries.remove(&id);
    }

    /// Update cached entry in-place if it exists. Used after write-back.
    fn update(&mut self, inode_ref: &InodeRef) {
        if self.entries.contains_key(&inode_ref.id) {
            self.entries.insert(inode_ref.id, inode_ref.clone());
        }
    }
}

/// The Ext4 filesystem implementation.
pub struct Ext4 {
    block_device: Arc<dyn BlockDevice>,
    /// Cached superblock to avoid repeated disk reads.
    /// The superblock is loaded once at mount time and updated
    /// in memory whenever it is written to disk.
    cached_super_block: spin::Mutex<SuperBlock>,
    /// Cached block group descriptors. Loaded at mount time.
    /// Index is block group id.
    cached_block_groups: Vec<spin::Mutex<BlockGroupDesc>>,
    /// Sorted, merged half-open ranges occupied by primary ext4 metadata.
    system_metadata_ranges: Vec<(PBlockId, PBlockId)>,
    /// LRU-ish inode cache. Avoids repeated disk reads for frequently accessed inodes.
    inode_cache: spin::Mutex<InodeCache>,
    /// Global allocation lock. Protects block/inode bitmap operations from
    /// concurrent modification, which would cause two inodes to receive the
    /// same physical block (corrupting extent trees and data).
    alloc_lock: spin::Mutex<()>,
    /// Serializes directory-entry/link-count transactions.  Inode data and
    /// writeback remain sharded; namespace operations are comparatively cold
    /// and need a single ordering domain until journal transactions exist.
    namespace_lock: spin::Mutex<()>,
    /// Separates legacy direct metadata writers from journal transactions.
    ///
    /// Direct writers hold a shared guard for their complete top-level
    /// operation. A journal transaction holds the exclusive guard from its
    /// first home-block snapshot until commit/cache publication. Taking this
    /// lock only at `write_block` would be too late: a transaction could have
    /// already captured an image which a direct writer subsequently changes.
    metadata_mutation_barrier: MetadataMutationGate,
    /// First unrecoverable metadata error.  Once set, mutation must fail-stop.
    poisoned: spin::Mutex<Option<ErrCode>>,
    /// Explicit metadata mutation lifecycle. Read-only probing cannot acquire
    /// either transaction backend and therefore cannot mutate or recover media.
    metadata_mode: MetadataMutationMode,
    /// Flush policy selected at mount. Journal mode currently requires this.
    write_barrier: bool,
    /// A Direct mount may restore VALID_FS only if it was set before mount.
    direct_restore_clean: bool,
    /// Serializes inode metadata and extent-tree mutations per inode shard.
    ///
    /// another_ext4 stores inodes as value snapshots in a small cache. Without
    /// this lock, two writers can clone the same cached inode, mutate disjoint
    /// fields, then write stale extent roots or sizes back over each other. Use
    /// sharding so unrelated apt download files do not serialize on one global
    /// filesystem-wide spin lock.
    inode_mutation_locks: Vec<spin::Mutex<()>>,
    prepare_stats: PrepareStats,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PrepareStatsSnapshot {
    pub enabled: bool,
    pub generation: usize,
    pub calls: usize,
    pub requested_blocks: usize,
    pub mapped_blocks: usize,
    pub missing_blocks: usize,
    pub failures: usize,
    pub elapsed_cycles: usize,
    pub bitmap_io: usize,
    pub gdt_io: usize,
    pub superblock_io: usize,
    pub inode_io: usize,
    pub extent_io: usize,
    pub zero_io: usize,
}

struct PrepareStats {
    generation: usize,
    calls: AtomicUsize,
    requested_blocks: AtomicUsize,
    mapped_blocks: AtomicUsize,
    missing_blocks: AtomicUsize,
    failures: AtomicUsize,
    elapsed_cycles: AtomicUsize,
    bitmap_io: AtomicUsize,
    gdt_io: AtomicUsize,
    superblock_io: AtomicUsize,
    inode_io: AtomicUsize,
    extent_io: AtomicUsize,
    zero_io: AtomicUsize,
}

impl PrepareStats {
    fn new() -> Self {
        Self {
            generation: NEXT_PREPARE_STATS_GENERATION.fetch_add(1, Ordering::Relaxed),
            calls: AtomicUsize::new(0),
            requested_blocks: AtomicUsize::new(0),
            mapped_blocks: AtomicUsize::new(0),
            missing_blocks: AtomicUsize::new(0),
            failures: AtomicUsize::new(0),
            elapsed_cycles: AtomicUsize::new(0),
            bitmap_io: AtomicUsize::new(0),
            gdt_io: AtomicUsize::new(0),
            superblock_io: AtomicUsize::new(0),
            inode_io: AtomicUsize::new(0),
            extent_io: AtomicUsize::new(0),
            zero_io: AtomicUsize::new(0),
        }
    }

    fn record_call(&self) {
        if P6_2_STATS_ENABLED {
            self.calls.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn record_requested(&self, blocks: usize) {
        if P6_2_STATS_ENABLED {
            self.requested_blocks.fetch_add(blocks, Ordering::Relaxed);
        }
    }

    fn record_mapped(&self) {
        if P6_2_STATS_ENABLED {
            self.mapped_blocks.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn record_missing(&self) {
        if P6_2_STATS_ENABLED {
            self.missing_blocks.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn record_missing_blocks(&self, blocks: usize) {
        if P6_2_STATS_ENABLED {
            self.missing_blocks.fetch_add(blocks, Ordering::Relaxed);
        }
    }

    fn record_failure(&self) {
        if P6_2_STATS_ENABLED {
            self.failures.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn record_elapsed_cycles(&self, cycles: usize) {
        if P6_2_STATS_ENABLED {
            self.elapsed_cycles.fetch_add(cycles, Ordering::Relaxed);
        }
    }

    fn record_bitmap_io(&self) {
        if P6_2_STATS_ENABLED {
            self.bitmap_io.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn record_gdt_io(&self) {
        if P6_2_STATS_ENABLED {
            self.gdt_io.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn record_superblock_io(&self) {
        if P6_2_STATS_ENABLED {
            self.superblock_io.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn record_inode_io(&self) {
        if P6_2_STATS_ENABLED {
            self.inode_io.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn record_extent_io(&self) {
        if P6_2_STATS_ENABLED {
            self.extent_io.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn record_zero_io(&self) {
        if P6_2_STATS_ENABLED {
            self.zero_io.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn snapshot(&self) -> PrepareStatsSnapshot {
        PrepareStatsSnapshot {
            enabled: P6_2_STATS_ENABLED,
            generation: self.generation,
            calls: self.calls.load(Ordering::Relaxed),
            requested_blocks: self.requested_blocks.load(Ordering::Relaxed),
            mapped_blocks: self.mapped_blocks.load(Ordering::Relaxed),
            missing_blocks: self.missing_blocks.load(Ordering::Relaxed),
            failures: self.failures.load(Ordering::Relaxed),
            elapsed_cycles: self.elapsed_cycles.load(Ordering::Relaxed),
            bitmap_io: self.bitmap_io.load(Ordering::Relaxed),
            gdt_io: self.gdt_io.load(Ordering::Relaxed),
            superblock_io: self.superblock_io.load(Ordering::Relaxed),
            inode_io: self.inode_io.load(Ordering::Relaxed),
            extent_io: self.extent_io.load(Ordering::Relaxed),
            zero_io: self.zero_io.load(Ordering::Relaxed),
        }
    }
}

pub(super) enum MetadataMutationMode {
    ReadOnly,
    Journal(journal_transaction::JournalTransactionCore),
    Direct(journal_transaction::DirectTransactionCore),
}

/// Non-blocking gate separating legacy direct writers from journal snapshots.
///
/// The top bit denotes an exclusive transactional owner; the remaining bits
/// count direct writers.  Acquisition never waits for an existing owner, which
/// is essential because guards intentionally span block-device I/O.
#[derive(Debug)]
struct MetadataMutationGate {
    state: AtomicUsize,
}

const METADATA_GATE_EXCLUSIVE: usize = 1usize << (usize::BITS - 1);
const METADATA_GATE_DIRECT_MAX: usize = METADATA_GATE_EXCLUSIVE - 1;
const P6_2_STATS_ENABLED: bool = option_env!("DRAGONOS_P6_2_STATS").is_some();
static NEXT_PREPARE_STATS_GENERATION: AtomicUsize = AtomicUsize::new(1);

impl MetadataMutationGate {
    const fn new() -> Self {
        Self {
            state: AtomicUsize::new(0),
        }
    }

    fn try_direct(&self) -> Result<MetadataMutationGuard<'_>> {
        const COMPATIBLE_CAS_RETRIES: usize = 64;
        let mut state = self.state.load(Ordering::Relaxed);
        for _ in 0..COMPATIBLE_CAS_RETRIES {
            if state & METADATA_GATE_EXCLUSIVE != 0 || state == METADATA_GATE_DIRECT_MAX {
                return Err(Ext4Error::new(ErrCode::EAGAIN));
            }
            match self.state.compare_exchange_weak(
                state,
                state + 1,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    return Ok(MetadataMutationGuard {
                        gate: self,
                        exclusive: false,
                    });
                }
                // Retry only a compatible direct-count collision. Observing an
                // exclusive owner is rejected at the top of the next iteration;
                // no acquisition waits for an I/O-spanning owner to depart.
                Err(observed) => state = observed,
            }
        }
        Err(Ext4Error::new(ErrCode::EAGAIN))
    }

    fn try_transactional(&self) -> Result<MetadataMutationGuard<'_>> {
        self.state
            .compare_exchange(
                0,
                METADATA_GATE_EXCLUSIVE,
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .map_err(|_| Ext4Error::new(ErrCode::EAGAIN))?;
        Ok(MetadataMutationGuard {
            gate: self,
            exclusive: true,
        })
    }
}

#[derive(Debug)]
pub(super) struct MetadataMutationGuard<'a> {
    gate: &'a MetadataMutationGate,
    exclusive: bool,
}

impl Drop for MetadataMutationGuard<'_> {
    fn drop(&mut self) {
        if self.exclusive {
            debug_assert_eq!(
                self.gate.state.load(Ordering::Relaxed),
                METADATA_GATE_EXCLUSIVE
            );
            self.gate.state.store(0, Ordering::Release);
        } else {
            let previous = self.gate.state.fetch_sub(1, Ordering::Release);
            debug_assert!(previous > 0 && previous < METADATA_GATE_EXCLUSIVE);
        }
    }
}

/// Maximum number of inodes to cache in memory.
const INODE_CACHE_SIZE: usize = 512;
pub(super) const INODE_MUTATION_LOCK_SHARDS: usize = 64;

impl Ext4 {
    pub fn prepare_stats_snapshot(&self) -> PrepareStatsSnapshot {
        self.prepare_stats.snapshot()
    }

    pub const fn prepare_stats_enabled(&self) -> bool {
        P6_2_STATS_ENABLED
    }

    pub fn record_prepare_elapsed_cycles(&self, cycles: usize) {
        self.prepare_stats.record_elapsed_cycles(cycles);
    }

    fn is_power_of(mut value: u32, base: u32) -> bool {
        while value > base && value.is_multiple_of(base) {
            value /= base;
        }
        value == base
    }

    fn block_group_has_super(sb: &SuperBlock, group: BlockGroupId) -> bool {
        if group == 0 {
            return true;
        }
        if sb.has_compatible_feature(SuperBlock::FEATURE_COMPAT_SPARSE_SUPER2) {
            return sb.backup_block_groups().contains(&group);
        }
        if group <= 1
            || !sb.has_read_only_compatible_feature(SuperBlock::FEATURE_RO_COMPAT_SPARSE_SUPER)
        {
            return true;
        }
        group & 1 != 0
            && (Self::is_power_of(group, 3)
                || Self::is_power_of(group, 5)
                || Self::is_power_of(group, 7))
    }

    fn merge_metadata_ranges(mut ranges: Vec<(PBlockId, PBlockId)>) -> Vec<(PBlockId, PBlockId)> {
        ranges.sort_unstable();
        let mut merged: Vec<(PBlockId, PBlockId)> = Vec::new();
        for (start, end) in ranges {
            if let Some(last) = merged.last_mut() {
                if start <= last.1 {
                    last.1 = core::cmp::max(last.1, end);
                    continue;
                }
            }
            merged.push((start, end));
        }
        merged
    }

    fn build_system_metadata_ranges(
        sb: &SuperBlock,
        groups: &[spin::Mutex<BlockGroupDesc>],
    ) -> Result<Vec<(PBlockId, PBlockId)>> {
        let desc_per_block = BLOCK_SIZE as u64 / sb.desc_size() as u64;
        let gdt_blocks = (sb.block_group_count() as u64).div_ceil(desc_per_block);
        let inode_table_blocks = (sb.inodes_per_group() as u64)
            .checked_mul(sb.inode_size() as u64)
            .and_then(|bytes| bytes.checked_add(BLOCK_SIZE as u64 - 1))
            .map(|bytes| bytes / BLOCK_SIZE as u64)
            .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;
        let mut ranges = Vec::new();
        ranges
            .try_reserve_exact(groups.len().saturating_mul(4))
            .map_err(|_| Ext4Error::new(ErrCode::ENOMEM))?;
        for (bgid, group) in groups.iter().enumerate() {
            let bgid = bgid as BlockGroupId;
            if Self::block_group_has_super(sb, bgid) {
                let start =
                    sb.first_data_block() as u64 + bgid as u64 * sb.blocks_per_group() as u64;
                let end = start
                    .checked_add(1)
                    .and_then(|value| value.checked_add(gdt_blocks))
                    .and_then(|value| value.checked_add(sb.reserved_gdt_blocks() as u64))
                    .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;
                let group_end = start
                    .checked_add(sb.blocks_per_group() as u64)
                    .map(|value| core::cmp::min(value, sb.block_count()))
                    .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;
                if end > group_end {
                    return Err(Ext4Error::new(ErrCode::EIO));
                }
                ranges.push((start, end));
            }
            let desc = *group.lock();
            for start in [desc.block_bitmap_block(), desc.inode_bitmap_block()] {
                ranges.push((
                    start,
                    start
                        .checked_add(1)
                        .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?,
                ));
            }
            let table = desc.inode_table_first_block();
            ranges.push((
                table,
                table
                    .checked_add(inode_table_blocks)
                    .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?,
            ));
        }
        Ok(Self::merge_metadata_ranges(ranges))
    }

    fn validate_super_block(sb: &SuperBlock) -> Result<()> {
        const SUPPORTED_INCOMPAT: u32 =
            0x0002 | 0x0004 | 0x0040 | 0x0080 | 0x0200 | SuperBlock::FEATURE_INCOMPAT_CSUM_SEED;
        const SUPPORTED_RO_COMPAT: u32 = 0x0001
            | 0x0002
            | 0x0004
            | 0x0008
            | 0x0020
            | 0x0040
            | 0x0400
            | SuperBlock::FEATURE_RO_COMPAT_ORPHAN_PRESENT;

        let metadata_csum =
            sb.has_read_only_compatible_feature(SuperBlock::FEATURE_RO_COMPAT_METADATA_CSUM);
        let blocks = sb.block_count();
        let blocks_per_group = sb.blocks_per_group() as u64;
        let inodes_per_group = sb.inodes_per_group() as u64;
        if !sb.check_magic()
            || (metadata_csum && (!sb.has_supported_checksum_type() || !sb.verify_checksum()))
            || sb.block_size() != BLOCK_SIZE as u64
            || sb.first_data_block() != 0
            || sb.inode_size() != SB_GOOD_INODE_SIZE
            || sb.desc_size() != SB_GOOD_DESC_SIZE
            || sb.incompatible_features() & !SUPPORTED_INCOMPAT != 0
            || sb.read_only_compatible_features() & !SUPPORTED_RO_COMPAT != 0
            || blocks_per_group == 0
            || inodes_per_group == 0
            || sb.clusters_per_group() == 0
            || sb.clusters_per_group() as usize > BLOCK_SIZE * 8
            || sb.inodes_per_group() as usize > BLOCK_SIZE * 8
            || sb.reserved_gdt_blocks() as usize > BLOCK_SIZE / 4
            || sb.clusters_per_group() != sb.blocks_per_group()
            || blocks <= sb.first_data_block() as u64
            || sb.inode_count() == 0
            || sb.first_inode() > sb.inode_count()
            || sb.free_blocks_count() > blocks
            || sb.free_inodes_count() > sb.inode_count()
            || sb.reserved_blocks_count() > blocks
        {
            return_error!(
                ErrCode::EIO,
                "Invalid ext4 superblock geometry, feature set, or checksum"
            );
        }

        let groups = (blocks - sb.first_data_block() as u64).div_ceil(blocks_per_group);
        let inode_capacity = groups
            .checked_mul(inodes_per_group)
            .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;
        let prior_capacity = groups
            .saturating_sub(1)
            .checked_mul(inodes_per_group)
            .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;
        if groups == 0
            || groups > u32::MAX as u64
            || sb.inode_count() as u64 > inode_capacity
            || sb.inode_count() as u64 <= prior_capacity
        {
            return_error!(ErrCode::EIO, "Invalid ext4 group-count or inode geometry");
        }
        Ok(())
    }

    fn read_validated_block_groups(
        device: &dyn BlockDevice,
        sb: &SuperBlock,
    ) -> Result<Vec<spin::Mutex<BlockGroupDesc>>> {
        Self::validate_super_block(sb)?;
        let bg_count = sb.block_group_count();
        let desc_per_block = BLOCK_SIZE as u32 / sb.desc_size() as u32;
        let metadata_csum =
            sb.has_read_only_compatible_feature(SuperBlock::FEATURE_RO_COMPAT_METADATA_CSUM);
        let inode_table_blocks = (sb.inodes_per_group() as u64)
            .checked_mul(sb.inode_size() as u64)
            .and_then(|bytes| bytes.checked_add(BLOCK_SIZE as u64 - 1))
            .map(|bytes| bytes / BLOCK_SIZE as u64)
            .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;
        let mut groups = Vec::new();
        groups
            .try_reserve_exact(bg_count as usize)
            .map_err(|_| Ext4Error::new(ErrCode::ENOMEM))?;
        for bgid in 0..bg_count {
            let block_id = sb
                .first_data_block()
                .checked_add(bgid / desc_per_block)
                .and_then(|id| id.checked_add(1))
                .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;
            if block_id as u64 >= sb.block_count() {
                return Err(Ext4Error::new(ErrCode::EIO));
            }
            let offset = (bgid % desc_per_block) * sb.desc_size() as u32;
            let block = device.read_block(block_id as PBlockId)?;
            let desc = block.read_offset_as::<BlockGroupDesc>(offset as usize);
            Self::validate_block_group(sb, bgid, &desc, metadata_csum, inode_table_blocks)?;
            groups.push(spin::Mutex::new(desc));
        }
        Ok(groups)
    }

    fn validate_block_group(
        sb: &SuperBlock,
        bgid: BlockGroupId,
        desc: &BlockGroupDesc,
        metadata_csum: bool,
        inode_table_blocks: u64,
    ) -> Result<()> {
        let group = BlockGroupRef::new(bgid, *desc);
        let block_bitmap = desc.block_bitmap_block();
        let inode_bitmap = desc.inode_bitmap_block();
        let inode_table = desc.inode_table_first_block();
        if (metadata_csum && !group.verify_checksum(sb.metadata_checksum_seed()))
            || block_bitmap == 0
            || block_bitmap >= sb.block_count()
            || inode_bitmap == 0
            || inode_bitmap >= sb.block_count()
            || inode_table == 0
            || inode_table >= sb.block_count()
            || inode_table
                .checked_add(inode_table_blocks)
                .is_none_or(|end| end > sb.block_count())
        {
            return_error!(ErrCode::EIO, "Invalid ext4 block-group descriptor");
        }
        Ok(())
    }

    /// Opens and loads an Ext4 from the `block_device`.
    pub fn load(block_device: Arc<dyn BlockDevice>) -> Result<Self> {
        // Load the superblock
        // TODO: if the main superblock is corrupted, should we load the backup?
        let block = block_device.read_block(0)?;
        let sb = block.read_offset_as::<SuperBlock>(BASE_OFFSET);
        log::debug!("Load Ext4 Superblock: {:?}", sb);
        let cached_block_groups = Self::read_validated_block_groups(block_device.as_ref(), &sb)?;
        let system_metadata_ranges = Self::build_system_metadata_ranges(&sb, &cached_block_groups)?;

        // Create Ext4 instance
        let mut inode_mutation_locks = Vec::with_capacity(INODE_MUTATION_LOCK_SHARDS);
        for _ in 0..INODE_MUTATION_LOCK_SHARDS {
            inode_mutation_locks.push(spin::Mutex::new(()));
        }
        Ok(Self {
            block_device,
            cached_super_block: spin::Mutex::new(sb),
            cached_block_groups,
            system_metadata_ranges,
            inode_cache: spin::Mutex::new(InodeCache::new(INODE_CACHE_SIZE)),
            alloc_lock: spin::Mutex::new(()),
            namespace_lock: spin::Mutex::new(()),
            metadata_mutation_barrier: MetadataMutationGate::new(),
            poisoned: spin::Mutex::new(None),
            metadata_mode: MetadataMutationMode::ReadOnly,
            write_barrier: true,
            direct_restore_clean: false,
            inode_mutation_locks,
            prepare_stats: PrepareStats::new(),
        })
    }

    /// Load, recover and activate a filesystem for metadata mutation.
    pub fn load_writable(block_device: Arc<dyn BlockDevice>) -> Result<Self> {
        Self::load_writable_with_options(block_device, true)
    }

    pub fn load_read_only_checked(block_device: Arc<dyn BlockDevice>) -> Result<Self> {
        let fs = Self::load(block_device)?;
        let sb = fs.read_super_block_cached();
        if sb.last_orphan() != 0
            || sb.has_read_only_compatible_feature(SuperBlock::FEATURE_RO_COMPAT_ORPHAN_PRESENT)
            || sb.has_incompatible_feature(SuperBlock::FEATURE_INCOMPAT_RECOVER)
        {
            return_error!(
                ErrCode::EROFS,
                "Filesystem requires recovery before read-only mount"
            );
        }
        Ok(fs)
    }

    pub fn load_writable_with_options(
        block_device: Arc<dyn BlockDevice>,
        write_barrier: bool,
    ) -> Result<Self> {
        let mut fs = Self::load(block_device)?;
        fs.write_barrier = write_barrier;
        fs.writable_orphan_preflight()?;
        let has_journal = fs
            .read_super_block_cached()
            .has_compatible_feature(SuperBlock::FEATURE_COMPAT_HAS_JOURNAL);
        if has_journal {
            if !write_barrier {
                return_error!(
                    ErrCode::ENOTSUP,
                    "barrier=0 is unsupported with the journal"
                );
            }
            fs.initialize_journal()?;
        } else {
            fs.initialize_direct()?;
            fs.mark_direct_mount_dirty()?;
            fs.cleanup_legacy_orphan_chain()?;
        }
        Ok(fs)
    }

    /// Initializes the root directory.
    pub fn init(&mut self) -> Result<()> {
        // Create root directory
        self.create_root_inode().map(|_| ())
    }

    /// Returns the current on-disk superblock.
    pub fn super_block(&self) -> Result<SuperBlock> {
        Ok(self.read_super_block_cached())
    }

    #[inline]
    fn inode_mutation_lock_index(&self, inode_id: InodeId) -> usize {
        inode_id as usize % self.inode_mutation_locks.len()
    }

    pub(super) fn ensure_mutable(&self) -> Result<()> {
        if matches!(self.metadata_mode, MetadataMutationMode::ReadOnly) {
            return_error!(ErrCode::EROFS, "Filesystem was opened read-only");
        }
        if let Some(code) = *self.poisoned.lock() {
            return_error!(code, "Filesystem is fail-stopped after a metadata error");
        }
        Ok(())
    }

    /// Prevent every subsequent metadata mutation after an upper-layer
    /// lifecycle invariant becomes indeterminate.
    pub fn fail_stop_mutations(&self) {
        self.poison(ErrCode::EIO);
    }

    fn ranges_overlap(
        start: PBlockId,
        end: PBlockId,
        reserved_start: PBlockId,
        reserved_end: PBlockId,
    ) -> bool {
        start < reserved_end && reserved_start < end
    }

    /// Validate that a physical range is ordinary file-owned storage rather
    /// than ext4 system metadata or the internal journal. All ranges are
    /// half-open and every end is checked before comparison.
    pub(super) fn validate_data_blocks(&self, start: PBlockId, count: u64) -> Result<()> {
        let sb = self.read_super_block_cached();
        let end = start
            .checked_add(count)
            .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;
        if count == 0 || start < sb.first_data_block() as u64 || end > sb.block_count() {
            return_error!(ErrCode::EIO, "Invalid data block range {}..{}", start, end);
        }

        let candidate = self
            .system_metadata_ranges
            .partition_point(|(_, reserved_end)| *reserved_end <= start);
        if self.system_metadata_ranges.get(candidate).is_some_and(
            |(reserved_start, reserved_end)| {
                Self::ranges_overlap(start, end, *reserved_start, *reserved_end)
            },
        ) {
            return_error!(ErrCode::EIO, "Data range overlaps ext4 system metadata");
        }
        if self.journal_owns_block_range(start, end) {
            return_error!(ErrCode::EIO, "Data range overlaps the internal journal");
        }
        Ok(())
    }

    pub(super) fn poison(&self, code: ErrCode) {
        let mut poisoned = self.poisoned.lock();
        if poisoned.is_none() {
            *poisoned = Some(code);
        }
    }

    pub(super) fn poison_on_error<T>(&self, result: Result<T>) -> Result<T> {
        if result.is_err() {
            self.poison(ErrCode::EIO);
        }
        result
    }

    pub(super) fn lock_inode_mutations(
        &self,
        inode_ids: &[InodeId],
    ) -> Vec<spin::MutexGuard<'_, ()>> {
        let mut indices: Vec<usize> = inode_ids
            .iter()
            .map(|inode_id| self.inode_mutation_lock_index(*inode_id))
            .collect();
        indices.sort_unstable();
        indices.dedup();
        indices
            .into_iter()
            .map(|index| self.inode_mutation_locks[index].lock())
            .collect()
    }

    /// Enter a complete legacy/direct metadata mutation operation.
    #[inline]
    pub(super) fn lock_direct_metadata_mutation(&self) -> Result<MetadataMutationGuard<'_>> {
        self.metadata_mutation_barrier.try_direct()
    }

    /// Enter a complete transaction-private snapshot/commit operation.
    ///
    /// Callers must not already hold a direct-mutation guard. Top-level
    /// operations which can choose a transactional path acquire this guard
    /// directly; contention is reported as `EAGAIN` rather than waited on.
    #[inline]
    pub(super) fn lock_transactional_metadata_mutation(&self) -> Result<MetadataMutationGuard<'_>> {
        self.metadata_mutation_barrier.try_transactional()
    }
}

#[cfg(test)]
mod validation_tests {
    use super::*;

    struct ValidationDevice {
        blocks: BTreeMap<PBlockId, Block>,
    }

    impl BlockDevice for ValidationDevice {
        fn read_block(&self, block_id: PBlockId) -> Result<Block> {
            self.blocks
                .get(&block_id)
                .cloned()
                .ok_or_else(|| Ext4Error::new(ErrCode::EIO))
        }

        fn write_block(&self, _block: &Block) -> Result<()> {
            Err(Ext4Error::new(ErrCode::EIO))
        }

        fn flush(&self) -> Result<()> {
            Ok(())
        }

        fn supports_reliable_flush(&self) -> bool {
            true
        }
    }

    fn validation_device(mut sb: SuperBlock) -> Arc<ValidationDevice> {
        sb.set_checksum();
        let mut desc = BlockGroupDesc::validation_fixture();
        let mut group = BlockGroupRef::new(0, desc);
        group.set_checksum(sb.metadata_checksum_seed());
        desc = group.desc;

        let mut block0 = Block::new(0, Box::new([0; BLOCK_SIZE]));
        block0.write_offset_as(BASE_OFFSET, &sb);
        let mut block1 = Block::new(1, Box::new([0; BLOCK_SIZE]));
        block1.write_offset_as(0, &desc);
        Arc::new(ValidationDevice {
            blocks: BTreeMap::from([(0, block0), (1, block1)]),
        })
    }

    fn system_zone_test_fs() -> Ext4 {
        Ext4::load(validation_device(SuperBlock::validation_fixture())).unwrap()
    }

    #[test]
    fn replayed_superblock_checksum_damage_is_rejected() {
        let mut sb = SuperBlock::validation_fixture();
        assert!(Ext4::validate_super_block(&sb).is_ok());
        sb.set_free_inodes_count(1);
        assert!(Ext4::validate_super_block(&sb).is_err());
    }

    #[test]
    fn reserved_gdt_blocks_obey_linux_geometry_limit() {
        let mut sb = SuperBlock::validation_fixture();
        sb.set_reserved_gdt_blocks((BLOCK_SIZE / 4) as u16);
        sb.set_checksum();
        assert!(Ext4::validate_super_block(&sb).is_ok());

        sb.set_reserved_gdt_blocks((BLOCK_SIZE / 4 + 1) as u16);
        sb.set_checksum();
        assert!(Ext4::validate_super_block(&sb).is_err());
    }

    #[test]
    fn orphan_present_is_recognized_by_superblock_validation() {
        let mut sb = SuperBlock::validation_fixture();
        sb.set_read_only_compatible_feature(SuperBlock::FEATURE_RO_COMPAT_ORPHAN_PRESENT, true);
        sb.set_checksum();

        assert!(Ext4::validate_super_block(&sb).is_ok());
    }

    #[test]
    fn read_only_load_rejects_pending_orphan_file_recovery() {
        let mut pending = SuperBlock::validation_fixture();
        pending
            .set_read_only_compatible_feature(SuperBlock::FEATURE_RO_COMPAT_ORPHAN_PRESENT, true);
        assert_eq!(
            Ext4::load_read_only_checked(validation_device(pending))
                .err()
                .unwrap()
                .code(),
            ErrCode::EROFS
        );

        let mut clean = SuperBlock::validation_fixture();
        clean.set_compatible_feature(SuperBlock::FEATURE_COMPAT_ORPHAN_FILE, true);
        assert!(Ext4::load_read_only_checked(validation_device(clean)).is_ok());
    }

    #[test]
    fn backup_super_groups_follow_linux_sparse_rules() {
        let mut sb = SuperBlock::validation_fixture();
        sb.set_read_only_compatible_feature(SuperBlock::FEATURE_RO_COMPAT_SPARSE_SUPER, true);
        for group in [0, 1, 3, 5, 7, 9, 25, 49] {
            assert!(Ext4::block_group_has_super(&sb, group));
        }
        for group in [2, 4, 11, 15, 21] {
            assert!(!Ext4::block_group_has_super(&sb, group));
        }

        sb.set_compatible_feature(SuperBlock::FEATURE_COMPAT_SPARSE_SUPER2, true);
        sb.set_backup_block_groups([4, 12]);
        assert!(Ext4::block_group_has_super(&sb, 0));
        assert!(Ext4::block_group_has_super(&sb, 4));
        assert!(Ext4::block_group_has_super(&sb, 12));
        assert!(!Ext4::block_group_has_super(&sb, 1));
        assert!(!Ext4::block_group_has_super(&sb, 3));
    }

    #[test]
    fn replayed_group_descriptor_damage_and_bad_addresses_are_rejected() {
        let sb = SuperBlock::validation_fixture();
        let mut desc = BlockGroupDesc::validation_fixture();
        let mut group = BlockGroupRef::new(0, desc);
        group.set_checksum(sb.metadata_checksum_seed());
        desc = group.desc;
        assert!(Ext4::validate_block_group(&sb, 0, &desc, true, 16).is_ok());

        desc.set_free_inodes_count(1);
        assert!(Ext4::validate_block_group(&sb, 0, &desc, true, 16).is_err());

        let mut bad_address = BlockGroupDesc::validation_fixture();
        let mut bytes = bad_address.to_bytes().to_vec();
        bytes[0..4].copy_from_slice(&(sb.block_count() as u32).to_le_bytes());
        bad_address = BlockGroupDesc::from_bytes(&bytes);
        let mut group = BlockGroupRef::new(0, bad_address);
        group.set_checksum(sb.metadata_checksum_seed());
        assert!(Ext4::validate_block_group(&sb, 0, &group.desc, true, 16).is_err());
    }

    #[test]
    fn malicious_extent_or_xattr_cannot_claim_system_metadata() {
        let fs = system_zone_test_fs();
        assert!(fs.validate_data_blocks(0, 1).is_err());
        assert!(fs.validate_data_blocks(1, 1).is_err());
        assert!(fs.validate_data_blocks(2, 1).is_err());
        assert!(fs.validate_data_blocks(3, 1).is_err());
        assert!(fs.validate_data_blocks(4, 16).is_err());
        assert!(fs.validate_data_blocks(20, 1).is_ok());
    }

    #[test]
    fn system_metadata_ranges_merge_adjacency_and_preserve_boundaries() {
        let merged = Ext4::merge_metadata_ranges(vec![(8, 10), (1, 3), (3, 5), (9, 12)]);
        assert_eq!(merged, vec![(1, 5), (8, 12)]);
        assert!(!Ext4::ranges_overlap(5, 8, 1, 5));
        assert!(!Ext4::ranges_overlap(5, 8, 8, 12));
        assert!(Ext4::ranges_overlap(4, 9, 1, 5));
    }
}
