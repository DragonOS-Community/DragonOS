use crate::constants::*;
use crate::ext4_defs::*;
use crate::prelude::*;
use crate::return_error;

mod alloc;
mod dir;
mod extent;
mod high_level;
mod journal;
mod journal_recovery;
mod journal_transaction;
mod link;
mod low_level;
mod rw;

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
    /// First unrecoverable metadata error.  Once set, mutation must fail-stop.
    poisoned: spin::Mutex<Option<ErrCode>>,
    /// Validated synchronous JBD2 engine. It is initialized only by
    /// `load_writable`; read-only probing must not mutate or recover media.
    journal: Option<journal_transaction::JournalTransactionCore>,
    /// Serializes inode metadata and extent-tree mutations per inode shard.
    ///
    /// another_ext4 stores inodes as value snapshots in a small cache. Without
    /// this lock, two writers can clone the same cached inode, mutate disjoint
    /// fields, then write stale extent roots or sizes back over each other. Use
    /// sharding so unrelated apt download files do not serialize on one global
    /// filesystem-wide spin lock.
    inode_mutation_locks: Vec<spin::Mutex<()>>,
}

/// Maximum number of inodes to cache in memory.
const INODE_CACHE_SIZE: usize = 512;
pub(super) const INODE_MUTATION_LOCK_SHARDS: usize = 64;

impl Ext4 {
    /// Opens and loads an Ext4 from the `block_device`.
    pub fn load(block_device: Arc<dyn BlockDevice>) -> Result<Self> {
        // Load the superblock
        // TODO: if the main superblock is corrupted, should we load the backup?
        let block = block_device.read_block(0)?;
        let sb = block.read_offset_as::<SuperBlock>(BASE_OFFSET);
        log::debug!("Load Ext4 Superblock: {:?}", sb);
        // Check magic number
        if !sb.check_magic() {
            return_error!(ErrCode::EINVAL, "Invalid magic number");
        }
        // Check inode size
        if sb.inode_size() != SB_GOOD_INODE_SIZE {
            return_error!(ErrCode::EINVAL, "Invalid inode size {}", sb.inode_size());
        }
        // Check block group desc size
        if sb.desc_size() != SB_GOOD_DESC_SIZE {
            return_error!(
                ErrCode::EINVAL,
                "Invalid block group desc size {}",
                sb.desc_size()
            );
        }

        // Load all block group descriptors into cache
        let bg_count = sb.block_group_count();
        let desc_per_block = BLOCK_SIZE as u32 / sb.desc_size() as u32;
        let mut cached_block_groups = Vec::with_capacity(bg_count as usize);
        for bgid in 0..bg_count {
            let block_id = sb.first_data_block() + bgid / desc_per_block + 1;
            let offset = (bgid % desc_per_block) * sb.desc_size() as u32;
            let bg_block = block_device.read_block(block_id as PBlockId)?;
            let desc = bg_block.read_offset_as::<BlockGroupDesc>(offset as usize);
            cached_block_groups.push(spin::Mutex::new(desc));
        }

        // Create Ext4 instance
        let mut inode_mutation_locks = Vec::with_capacity(INODE_MUTATION_LOCK_SHARDS);
        for _ in 0..INODE_MUTATION_LOCK_SHARDS {
            inode_mutation_locks.push(spin::Mutex::new(()));
        }
        Ok(Self {
            block_device,
            cached_super_block: spin::Mutex::new(sb),
            cached_block_groups,
            inode_cache: spin::Mutex::new(InodeCache::new(INODE_CACHE_SIZE)),
            alloc_lock: spin::Mutex::new(()),
            namespace_lock: spin::Mutex::new(()),
            poisoned: spin::Mutex::new(None),
            journal: None,
            inode_mutation_locks,
        })
    }

    /// Load, recover and activate a filesystem for metadata mutation.
    pub fn load_writable(block_device: Arc<dyn BlockDevice>) -> Result<Self> {
        let mut fs = Self::load(block_device)?;
        fs.initialize_journal()?;
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
        if let Some(code) = *self.poisoned.lock() {
            return_error!(code, "Filesystem is fail-stopped after a metadata error");
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
}
