mod directory;
mod file;
mod vfs;

use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::mem::size_of;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use system_error::SystemError;

use crate::{
    driver::base::device::device_number::DeviceNumber,
    filesystem::{
        page_cache::PageCache,
        vfs::{FileType, InodeFlags, InodeId, InodeMode, Metadata},
    },
    libs::{
        mutex::Mutex,
        rwsem::{RwSem, RwSemReadGuard, RwSemWriteGuard},
        wait_queue::WaitQueue,
    },
    mm::{fault::FaultRetryWait, MemoryManagementArch},
    time::{jiffies::NSEC_PER_JIFFY, timer::clock, PosixTimeSpec, NSEC_PER_SEC},
};

use super::reply::FuseReply;
use super::{
    conn::FuseConn,
    fs::FuseFS,
    private_data::FuseWritebackHandle,
    protocol::{
        fuse_pack_struct, fuse_read_struct, FuseAttr, FuseAttrOut, FuseEntryOut, FuseGetattrIn,
        FuseWriteIn, FuseWriteOut, FUSE_GETATTR, FUSE_GETATTR_FH, FUSE_ROOT_ID, FUSE_WRITE,
        FUSE_WRITE_LOCKOWNER,
    },
};

use super::{
    private_data::FuseOpenPrivateData,
    virtiofs::dax::{
        DaxAdmissionGuard, DaxAdmissionState, DaxMappingOwner, DaxRangeAllocator, OwnedToken,
        ReclaimCandidate, ReclaimToken, DAX_RANGE_SIZE,
    },
};

static NEXT_NODE_INCARNATION: AtomicU64 = AtomicU64::new(1);

fn getattr_request_input(fh: Option<u64>) -> FuseGetattrIn {
    FuseGetattrIn {
        getattr_flags: if fh.is_some() { FUSE_GETATTR_FH } else { 0 },
        dummy: 0,
        fh: fh.unwrap_or(0),
    }
}

#[derive(Debug)]
pub(crate) struct DaxMapping {
    file_offset: u64,
    writable: AtomicBool,
    token: OwnedToken,
}

#[derive(Debug, Default)]
struct DaxMappingTree {
    mappings: BTreeMap<u64, Arc<DaxMapping>>,
    tombstones: BTreeMap<u64, Arc<DaxMapping>>,
    sequence: u64,
}

#[derive(Debug)]
pub(crate) struct DaxAccessGuard {
    mapping: Arc<DaxMapping>,
    allocator: Arc<DaxRangeAllocator>,
    window: Arc<crate::driver::virtio::virtio_fs::VirtioFsCacheWindow>,
    _admission: Option<DaxAdmissionGuard>,
}

#[derive(Debug)]
pub(crate) struct DaxReclaimTombstone {
    file_offset: u64,
    mapping: Arc<DaxMapping>,
    token: ReclaimToken,
}

/// Blocks host-invalidated DAX contents from being consumed while keeping
/// daemon requests outside the bounded active section.  A notification can
/// therefore drain actual window users without waiting for a SETUPMAPPING
/// request whose reply depends on that same notification returning.
#[derive(Debug)]
struct DaxHostInvalidationGate {
    blockers: AtomicUsize,
    active: AtomicUsize,
    wait: WaitQueue,
}

#[derive(Debug)]
pub(crate) struct DaxHostAccessGuard {
    gate: Arc<DaxHostInvalidationGate>,
}

#[derive(Debug)]
pub(crate) struct DaxHostInvalidationBlocker {
    gate: Arc<DaxHostInvalidationGate>,
}

#[derive(Debug)]
struct DaxHostInvalidationRetryWait {
    gate: Arc<DaxHostInvalidationGate>,
}

impl DaxHostInvalidationGate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            blockers: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            wait: WaitQueue::default(),
        })
    }

    fn try_enter(self: &Arc<Self>) -> Result<DaxHostAccessGuard, SystemError> {
        if self.blockers.load(Ordering::Acquire) != 0 {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }
        self.active.fetch_add(1, Ordering::AcqRel);
        if self.blockers.load(Ordering::Acquire) != 0 {
            self.leave_active();
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }
        Ok(DaxHostAccessGuard { gate: self.clone() })
    }

    fn begin(self: &Arc<Self>) -> Result<DaxHostInvalidationBlocker, SystemError> {
        self.blockers
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                count.checked_add(1)
            })
            .map_err(|_| SystemError::EOVERFLOW)?;
        self.wait
            .wait_until(|| (self.active.load(Ordering::Acquire) == 0).then_some(()));
        Ok(DaxHostInvalidationBlocker { gate: self.clone() })
    }

    fn blocked(&self) -> bool {
        self.blockers.load(Ordering::Acquire) != 0
    }

    fn wait_unblocked(&self) {
        self.wait.wait_until(|| (!self.blocked()).then_some(()));
    }

    fn wait_unblocked_interruptible(&self) -> Result<(), SystemError> {
        self.wait
            .wait_until_interruptible(|| (!self.blocked()).then_some(()))
    }

    fn leave_active(&self) {
        if self.active.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.wait.wakeup_all(None);
        }
    }

    fn leave_blocker(&self) {
        let previous = self.blockers.fetch_sub(1, Ordering::AcqRel);
        debug_assert_ne!(previous, 0);
        if previous == 1 {
            self.wait.wakeup_all(None);
        }
    }
}

impl Drop for DaxHostAccessGuard {
    fn drop(&mut self) {
        self.gate.leave_active();
    }
}

impl Drop for DaxHostInvalidationBlocker {
    fn drop(&mut self) {
        self.gate.leave_blocker();
    }
}

impl FaultRetryWait for DaxHostInvalidationRetryWait {
    fn wait(&self) -> Result<(), SystemError> {
        self.gate.wait_unblocked();
        Ok(())
    }
}

impl DaxMappingTree {
    fn bump_sequence(&mut self) -> Result<(), SystemError> {
        self.sequence = self.sequence.checked_add(1).ok_or(SystemError::EOVERFLOW)?;
        Ok(())
    }
}

impl DaxMapping {
    pub(crate) fn file_offset(&self) -> u64 {
        self.file_offset
    }

    pub(crate) fn window_offset(&self) -> usize {
        self.token.window_offset()
    }

    pub(crate) fn writable(&self) -> bool {
        self.writable.load(Ordering::Acquire)
    }

    pub(crate) fn owner(&self) -> DaxMappingOwner {
        self.token.owner()
    }
}

impl DaxAccessGuard {
    pub(crate) fn mapping(&self) -> &Arc<DaxMapping> {
        &self.mapping
    }

    fn checked_window_offset(&self, offset: usize, len: usize) -> Result<usize, SystemError> {
        let end = offset.checked_add(len).ok_or(SystemError::EOVERFLOW)?;
        if end > DAX_RANGE_SIZE {
            return Err(SystemError::ERANGE);
        }
        self.mapping
            .window_offset()
            .checked_add(offset)
            .ok_or(SystemError::EOVERFLOW)
    }

    pub(crate) fn checked_paddr(
        &self,
        offset: usize,
        len: usize,
    ) -> Result<crate::mm::PhysAddr, SystemError> {
        self.window
            .checked_paddr(self.checked_window_offset(offset, len)?, len)
    }

    pub(crate) fn copy_to(&self, offset: usize, dst: &mut [u8]) -> Result<(), SystemError> {
        let window_offset = self.checked_window_offset(offset, dst.len())?;
        let src = self.window.checked_vaddr(window_offset, dst.len())?;
        unsafe {
            core::ptr::copy_nonoverlapping(src.data() as *const u8, dst.as_mut_ptr(), dst.len());
        }
        Ok(())
    }

    pub(crate) fn copy_from(&self, offset: usize, src: &[u8]) -> Result<(), SystemError> {
        if !self.mapping.writable() {
            return Err(SystemError::EACCES);
        }
        let window_offset = self.checked_window_offset(offset, src.len())?;
        let dst = self.window.checked_vaddr(window_offset, src.len())?;
        unsafe {
            core::ptr::copy_nonoverlapping(src.as_ptr(), dst.data() as *mut u8, src.len());
        }
        Ok(())
    }
}

impl Drop for DaxAccessGuard {
    fn drop(&mut self) {
        let result = self.allocator.put(&self.mapping.token);
        debug_assert!(result.is_ok() || result == Err(SystemError::EINVAL));
    }
}

#[derive(Debug)]
pub struct FuseNode {
    fs: Weak<FuseFS>,
    conn: Arc<FuseConn>,
    self_ref: Weak<FuseNode>,
    nodeid: u64,
    /// Kernel-local identity for this FuseNode object.  The daemon generation
    /// alone is insufficient because a type mismatch retires a node even when
    /// the daemon accidentally reuses the same (nodeid, generation) pair.
    node_incarnation: u64,
    parent_nodeid: Mutex<u64>,
    parent: Mutex<Option<Arc<FuseNode>>>,
    name: Mutex<Option<String>>,
    cached_metadata: Mutex<Option<Metadata>>,
    page_cache: Mutex<Option<Arc<PageCache>>>,
    writeback_handles: Mutex<Vec<Arc<FuseWritebackHandle>>>,
    lookup_cache: Mutex<BTreeMap<String, FuseLookupCacheEntry>>,
    /// Serializes dirty-page admission against operations which must drain and
    /// invalidate the page cache (truncate and direct I/O).
    writeback_barrier: RwSem<()>,
    /// Serializes the complete two-zap/tombstone/REMOVEMAPPING transaction.
    /// The layout semaphore is intentionally dropped between the two zaps, so
    /// it cannot by itself prevent another layout breaker from observing and
    /// passing a half-finished tombstone.
    dax_reclaim_serial: Mutex<()>,
    dax_mappings: RwSem<DaxMappingTree>,
    dax_host_invalidation: Arc<DaxHostInvalidationGate>,
    dax_pte_epoch: AtomicU64,
    cached_metadata_deadline_ticks: AtomicU64,
    attr_version: AtomicU64,
    /// Version chain produced while short READ replies from one metadata
    /// snapshot monotonically converge on the lowest observed EOF.
    short_read_source_attr_version: AtomicU64,
    short_read_chain_attr_version: AtomicU64,
    /// Lowest EOF established by a short READ whose inode-wide cache truncate
    /// is deferred out of the transport completion path.
    pending_short_read_eof: AtomicU64,
    lookup_count: AtomicU64,
    /// 最近一次 LOOKUP 回复中的 fuse_attr.flags（用于 announce-submounts）。
    lookup_attr_flags: AtomicU32,
    /// Fixed for this FuseNode incarnation, matching Linux inode S_DAX.
    dax_active: bool,
    /// Linux d_mark_dontcache equivalent for a per-inode DAX attribute change.
    dax_dontcache: AtomicBool,
    /// LOOKUP 返回的 generation，用于检测 virtiofsd 复用 nodeid。
    generation: AtomicU64,
    stale: AtomicBool,
}

#[derive(Debug, Clone)]
struct FuseLookupCacheEntry {
    child: Arc<FuseNode>,
    generation: u64,
    deadline_ticks: u64,
}

impl FuseNode {
    const FUSE_DIRENT_ALIGN: usize = 8;
    const LOOKUP_CACHE_MAX_ENTRIES: usize = 1024;
    const READDIR_BUFFER_SIZE: usize = 64 * 1024;
    const XATTR_SIZE_MAX: usize = 65536;
    const XATTR_LIST_MAX: usize = 65536;

    pub fn new(
        fs: Weak<FuseFS>,
        conn: Arc<FuseConn>,
        nodeid: u64,
        parent_nodeid: u64,
        parent: Option<Arc<FuseNode>>,
        mut cached: Option<Metadata>,
        attr_flags: u32,
    ) -> Arc<Self> {
        let regular = cached
            .as_ref()
            .is_some_and(|md| md.file_type == FileType::File);
        let dax_active = conn.dax_inode_active(attr_flags, regular);
        if let Some(md) = cached.as_mut() {
            if dax_active {
                md.flags.insert(InodeFlags::S_DAX);
            } else {
                md.flags.remove(InodeFlags::S_DAX);
            }
        }
        let has_cached = cached.is_some();
        let initial_attr_epoch = conn.sample_attr_epoch();
        let node_incarnation = NEXT_NODE_INCARNATION.fetch_add(1, Ordering::Relaxed);
        assert_ne!(node_incarnation, 0, "FUSE node identity exhausted");
        let node = Arc::new_cyclic(|self_ref| Self {
            fs,
            conn,
            self_ref: self_ref.clone(),
            nodeid,
            node_incarnation,
            parent_nodeid: Mutex::new(parent_nodeid),
            parent: Mutex::new(parent),
            name: Mutex::new(None),
            cached_metadata: Mutex::new(cached),
            page_cache: Mutex::new(None),
            writeback_handles: Mutex::new(Vec::new()),
            lookup_cache: Mutex::new(BTreeMap::new()),
            writeback_barrier: RwSem::new(()),
            dax_reclaim_serial: Mutex::new(()),
            dax_mappings: RwSem::new(DaxMappingTree::default()),
            dax_host_invalidation: DaxHostInvalidationGate::new(),
            dax_pte_epoch: AtomicU64::new(0),
            cached_metadata_deadline_ticks: AtomicU64::new(if has_cached { u64::MAX } else { 0 }),
            attr_version: AtomicU64::new(initial_attr_epoch),
            short_read_source_attr_version: AtomicU64::new(0),
            short_read_chain_attr_version: AtomicU64::new(0),
            pending_short_read_eof: AtomicU64::new(u64::MAX),
            lookup_count: AtomicU64::new(0),
            lookup_attr_flags: AtomicU32::new(0),
            dax_active,
            dax_dontcache: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            stale: AtomicBool::new(false),
        });
        if node.dax_active {
            node.conn.register_dax_node(&node);
        }
        node
    }

    pub fn lookup_attr_flags(&self) -> u32 {
        self.lookup_attr_flags.load(Ordering::Relaxed)
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    pub(crate) fn node_incarnation(&self) -> u64 {
        self.node_incarnation
    }

    pub(crate) fn dax_mapping_owner(&self) -> super::virtiofs::dax::DaxMappingOwner {
        super::virtiofs::dax::DaxMappingOwner::from_inode(self.nodeid, self.node_incarnation)
            .expect("FuseNode has a valid DAX owner identity")
    }

    pub(crate) fn dax_active(&self) -> bool {
        self.dax_active
    }

    pub(crate) fn dax_dontcache(&self) -> bool {
        self.dax_dontcache.load(Ordering::Acquire)
    }

    pub(crate) fn set_lookup_attr_flags(&self, flags: u32) {
        self.lookup_attr_flags.store(flags, Ordering::Relaxed);
    }

    fn metadata_with_dax_state(&self, mut md: Metadata) -> Metadata {
        if self.dax_active {
            md.flags.insert(InodeFlags::S_DAX);
        } else {
            md.flags.remove(InodeFlags::S_DAX);
        }
        md
    }

    fn note_dax_attr_change(&self, attr_flags: u32) {
        if !self.conn.dax_mode().attr_change_requires_dontcache(
            self.dax_active,
            (attr_flags & super::protocol::FUSE_ATTR_DAX) != 0,
        ) || self
            .dax_dontcache
            .compare_exchange(false, true, Ordering::Release, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let Some(node) = self.self_ref.upgrade() else {
            return;
        };
        if let Some(fs) = self.fs.upgrade() {
            fs.purge_lookup_aliases(&node);
        }
    }

    pub(crate) fn dax_layout_read(&self) -> RwSemReadGuard<'_, ()> {
        self.writeback_barrier.read()
    }

    pub(crate) fn dax_layout_write(&self) -> RwSemWriteGuard<'_, ()> {
        self.writeback_barrier.write()
    }

    pub(crate) fn dax_try_host_access(&self) -> Result<DaxHostAccessGuard, SystemError> {
        self.dax_host_invalidation.try_enter()
    }

    pub(crate) fn dax_begin_host_invalidation(
        &self,
    ) -> Result<DaxHostInvalidationBlocker, SystemError> {
        self.dax_host_invalidation.begin()
    }

    pub(crate) fn dax_host_invalidation_blocked(&self) -> bool {
        self.dax_host_invalidation.blocked()
    }

    pub(crate) fn dax_wait_host_invalidation_interruptible(&self) -> Result<(), SystemError> {
        self.dax_host_invalidation.wait_unblocked_interruptible()
    }

    pub(crate) fn dax_host_invalidation_retry_wait(&self) -> Arc<dyn FaultRetryWait> {
        Arc::new(DaxHostInvalidationRetryWait {
            gate: self.dax_host_invalidation.clone(),
        })
    }

    pub(crate) fn dax_pte_epoch(&self) -> u64 {
        self.dax_pte_epoch.load(Ordering::Acquire)
    }

    pub(crate) fn dax_note_pte_published(&self) -> Result<u64, SystemError> {
        self.dax_pte_epoch
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |epoch| {
                epoch.checked_add(1)
            })
            .map(|old| old + 1)
            .map_err(|_| SystemError::EOVERFLOW)
    }

    fn dax_aligned_offset(offset: u64) -> u64 {
        offset & !(DAX_RANGE_SIZE as u64 - 1)
    }

    pub(crate) fn dax_access(
        &self,
        offset: u64,
        writable: bool,
    ) -> Result<DaxAccessGuard, SystemError> {
        let admission = self.conn.enter_dax()?;
        self.dax_access_inner(offset, writable, Some(admission))
    }

    fn dax_access_inner(
        &self,
        offset: u64,
        writable: bool,
        admission: Option<DaxAdmissionGuard>,
    ) -> Result<DaxAccessGuard, SystemError> {
        let file_offset = Self::dax_aligned_offset(offset);
        let allocator = self
            .conn
            .dax_allocator()
            .cloned()
            .ok_or(SystemError::EOPNOTSUPP_OR_ENOTSUP)?;
        let window = self.conn.dax_window()?;

        loop {
            let mapping = {
                let tree = self.dax_mappings.read();
                if tree.tombstones.contains_key(&file_offset) {
                    return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                }
                let mapping = tree.mappings.get(&file_offset).cloned();
                if let Some(mapping) = mapping.as_ref() {
                    // Pin while the tree read lock still prevents reclaim from
                    // transitioning this exact token to Reclaiming.
                    allocator.get(&mapping.token)?;
                }
                mapping
            };

            if let Some(mapping) = mapping {
                if !writable || mapping.writable() {
                    return Ok(DaxAccessGuard {
                        mapping,
                        allocator,
                        window,
                        _admission: admission,
                    });
                }

                let mut tree = self.dax_mappings.write();
                let current = tree.mappings.get(&file_offset).cloned();
                if !current
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &mapping))
                {
                    drop(tree);
                    allocator.put(&mapping.token)?;
                    continue;
                }
                if !mapping.writable() {
                    if let Err(error) = self.conn.setup_existing_dax_mapping(
                        self.dax_mapping_owner(),
                        &mapping.token,
                        file_offset,
                        true,
                    ) {
                        drop(tree);
                        allocator.put(&mapping.token)?;
                        return Err(error);
                    }
                    mapping.writable.store(true, Ordering::Release);
                    tree.bump_sequence()?;
                }
                drop(tree);
                return Ok(DaxAccessGuard {
                    mapping,
                    allocator,
                    window,
                    _admission: admission,
                });
            }

            let mut tree = self.dax_mappings.write();
            if tree.tombstones.contains_key(&file_offset) {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
            if tree.mappings.contains_key(&file_offset) {
                drop(tree);
                continue;
            }
            let token =
                self.conn
                    .setup_dax_mapping(self.dax_mapping_owner(), file_offset, writable)?;
            let mapping = Arc::new(DaxMapping {
                file_offset,
                writable: AtomicBool::new(writable),
                token,
            });
            // The daemon mapping exists but cannot safely be published locally
            // without the access reference. Retain allocator ownership on error;
            // teardown will retire the connection range.
            allocator.get(&mapping.token)?;
            tree.bump_sequence()?;
            tree.mappings.insert(file_offset, mapping.clone());
            drop(tree);
            return Ok(DaxAccessGuard {
                mapping,
                allocator,
                window,
                _admission: admission,
            });
        }
    }

    pub(crate) fn dax_isolate_reclaim(
        &self,
        candidate: &ReclaimCandidate,
        expected_pte_epoch: u64,
    ) -> Result<DaxReclaimTombstone, SystemError> {
        let _layout = self.dax_layout_write();
        self.dax_isolate_reclaim_locked(candidate, expected_pte_epoch)
    }

    fn dax_isolate_reclaim_locked(
        &self,
        candidate: &ReclaimCandidate,
        expected_pte_epoch: u64,
    ) -> Result<DaxReclaimTombstone, SystemError> {
        if self.dax_pte_epoch() != expected_pte_epoch {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }
        let allocator = self
            .conn
            .dax_allocator()
            .ok_or(SystemError::EOPNOTSUPP_OR_ENOTSUP)?;
        let mut tree = self.dax_mappings.write();
        let Some((file_offset, mapping)) = tree
            .mappings
            .iter()
            .find(|(_, mapping)| mapping.token.matches_candidate(candidate))
            .map(|(offset, mapping)| (*offset, mapping.clone()))
        else {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        };
        let token = allocator.begin_reclaim(candidate)?;
        tree.mappings.remove(&file_offset);
        tree.tombstones.insert(file_offset, mapping.clone());
        tree.bump_sequence()?;
        Ok(DaxReclaimTombstone {
            file_offset,
            mapping,
            token,
        })
    }

    fn dax_mapping_intersects(
        mapping: &DaxMapping,
        start: usize,
        end_exclusive: Option<usize>,
    ) -> Result<bool, SystemError> {
        let mapping_start =
            usize::try_from(mapping.file_offset).map_err(|_| SystemError::EOVERFLOW)?;
        let mapping_end = mapping_start
            .checked_add(DAX_RANGE_SIZE)
            .ok_or(SystemError::EOVERFLOW)?;
        Ok(mapping_end > start && end_exclusive.is_none_or(|end| mapping_start < end))
    }

    /// Acquire the inode layout exclusively after revoking and removing every
    /// DAX range intersecting `[start, end_exclusive)`. Sequence and PTE epochs
    /// make the rmap-walk-to-lock transition retryable instead of racy.
    fn dax_layout_write_for_range_with_restore(
        &self,
        start: usize,
        end_exclusive: Option<usize>,
        restore_on_failure: bool,
    ) -> Result<RwSemWriteGuard<'_, ()>, SystemError> {
        if !self.dax_active() {
            return Ok(self.dax_layout_write());
        }
        let _reclaim_serial = self.dax_reclaim_serial.lock();
        'retry: loop {
            let page_cache = self.cached_page_cache();
            let (sequence, mappings) = {
                let tree = self.dax_mappings.read();
                let mut mappings = Vec::new();
                for mapping in tree.mappings.values() {
                    if Self::dax_mapping_intersects(mapping, start, end_exclusive)? {
                        mappings.push(mapping.clone());
                    }
                }
                (tree.sequence, mappings)
            };
            let epoch = self.dax_pte_epoch();
            if let Some(cache) = page_cache.as_ref() {
                for mapping in &mappings {
                    let start = usize::try_from(mapping.file_offset)
                        .map_err(|_| SystemError::EOVERFLOW)?
                        >> crate::arch::MMArch::PAGE_SHIFT;
                    let end = start
                        .checked_add(DAX_RANGE_SIZE >> crate::arch::MMArch::PAGE_SHIFT)
                        .ok_or(SystemError::EOVERFLOW)?;
                    cache.unmap_mapping_pages(start, Some(end))?;
                }
            }
            let layout = self.dax_layout_write();
            if self.dax_pte_epoch() != epoch || self.dax_mappings.read().sequence != sequence {
                drop(layout);
                continue;
            }
            let allocator = self
                .conn
                .dax_allocator()
                .ok_or(SystemError::EOPNOTSUPP_OR_ENOTSUP)?;
            let total = allocator.snapshot().total;
            let mut candidates = Vec::with_capacity(total);
            allocator.reclaim_candidates(&mut candidates, total)?;
            let mut tombstones = Vec::with_capacity(mappings.len());
            for mapping in &mappings {
                let Some(candidate) = candidates
                    .iter()
                    .find(|candidate| mapping.token.matches_candidate(candidate))
                else {
                    for tombstone in tombstones.drain(..) {
                        let _ = self.dax_cancel_reclaim(tombstone);
                    }
                    drop(layout);
                    continue 'retry;
                };
                match self.dax_isolate_reclaim_locked(candidate, epoch) {
                    Ok(tombstone) => tombstones.push(tombstone),
                    Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                        for tombstone in tombstones.drain(..) {
                            let _ = self.dax_cancel_reclaim(tombstone);
                        }
                        drop(layout);
                        continue 'retry;
                    }
                    Err(error) => {
                        for tombstone in tombstones.drain(..) {
                            let _ = self.dax_cancel_reclaim(tombstone);
                        }
                        return Err(error);
                    }
                }
            }
            drop(layout);
            let second_zap = (|| {
                for tombstone in &tombstones {
                    if let Some(cache) = page_cache.as_ref() {
                        let start = usize::try_from(tombstone.file_offset)
                            .map_err(|_| SystemError::EOVERFLOW)?
                            >> crate::arch::MMArch::PAGE_SHIFT;
                        let end = start
                            .checked_add(DAX_RANGE_SIZE >> crate::arch::MMArch::PAGE_SHIFT)
                            .ok_or(SystemError::EOVERFLOW)?;
                        // The tombstone blocks new DAX faults. A second rmap
                        // pass drains fork/mremap publishers that raced the
                        // first zap.
                        cache.unmap_mapping_pages(start, Some(end))?;
                    }
                }
                Ok::<(), SystemError>(())
            })();
            if let Err(error) = second_zap {
                for tombstone in tombstones.drain(..) {
                    let _ = self.dax_cancel_reclaim(tombstone);
                }
                return Err(error);
            }
            let layout = self.dax_layout_write();
            let mut remaining = tombstones.into_iter();
            while let Some(tombstone) = remaining.next() {
                if let Err(error) =
                    self.dax_finish_reclaim_with_restore(tombstone, restore_on_failure)
                {
                    for unsubmitted in remaining {
                        if restore_on_failure {
                            let _ = self.dax_cancel_reclaim(unsubmitted);
                        }
                    }
                    return Err(error);
                }
            }
            return Ok(layout);
        }
    }

    pub(crate) fn dax_layout_write_for_range(
        &self,
        start: usize,
        end_exclusive: Option<usize>,
    ) -> Result<RwSemWriteGuard<'_, ()>, SystemError> {
        self.dax_layout_write_for_range_with_restore(start, end_exclusive, true)
    }

    pub(crate) fn dax_layout_write_for_truncate(
        &self,
        new_size: usize,
    ) -> Result<RwSemWriteGuard<'_, ()>, SystemError> {
        self.dax_layout_write_for_range(new_size, None)
    }

    pub(crate) fn dax_layout_write_for_all(&self) -> Result<RwSemWriteGuard<'_, ()>, SystemError> {
        self.dax_layout_write_for_range(0, None)
    }

    pub(crate) fn dax_layout_write_for_host_invalidation(
        &self,
        start: usize,
        end_exclusive: Option<usize>,
    ) -> Result<RwSemWriteGuard<'_, ()>, SystemError> {
        self.dax_layout_write_for_range_with_restore(start, end_exclusive, false)
    }

    pub(crate) fn dax_teardown(&self) -> Result<(), SystemError> {
        if self.dax_active() {
            drop(self.dax_layout_write_for_all()?);
        }
        Ok(())
    }

    /// Disconnect-only local revocation. The daemon can no longer confirm
    /// REMOVEMAPPING, so clear inode ownership after two PTE zaps and let the
    /// connection retire every allocator entry globally.
    pub(crate) fn dax_disconnect_revoke(&self) -> Result<(), SystemError> {
        let page_cache = self.cached_page_cache();
        let mappings = self
            .dax_mappings
            .read()
            .mappings
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let zap = |mapping: &DaxMapping| -> Result<(), SystemError> {
            let Some(cache) = page_cache.as_ref() else {
                return Ok(());
            };
            let start = usize::try_from(mapping.file_offset).map_err(|_| SystemError::EOVERFLOW)?
                >> crate::arch::MMArch::PAGE_SHIFT;
            let end = start
                .checked_add(DAX_RANGE_SIZE >> crate::arch::MMArch::PAGE_SHIFT)
                .ok_or(SystemError::EOVERFLOW)?;
            cache.unmap_mapping_pages(start, Some(end))
        };
        for mapping in &mappings {
            zap(mapping)?;
        }
        {
            let _layout = self.dax_layout_write();
            let mut tree = self.dax_mappings.write();
            tree.mappings.clear();
            tree.tombstones.clear();
            tree.bump_sequence()?;
        }
        for mapping in &mappings {
            zap(mapping)?;
        }
        Ok(())
    }

    fn dax_abandon_mappings(&self) -> Result<(), SystemError> {
        let mappings = self
            .dax_mappings
            .read()
            .mappings
            .values()
            .cloned()
            .collect::<Vec<_>>();
        self.dax_disconnect_revoke()?;
        let Some(allocator) = self.conn.dax_allocator() else {
            return Ok(());
        };
        let total = allocator.snapshot().total;
        let mut candidates = Vec::with_capacity(total);
        allocator.reclaim_candidates(&mut candidates, total)?;
        for mapping in mappings {
            if let Some(candidate) = candidates
                .iter()
                .find(|candidate| mapping.token.matches_candidate(candidate))
            {
                let token = allocator.begin_reclaim(candidate)?;
                allocator.retire_reclaim(&token)?;
            }
        }
        Ok(())
    }

    pub(crate) fn dax_cancel_reclaim(
        &self,
        tombstone: DaxReclaimTombstone,
    ) -> Result<(), SystemError> {
        let allocator = self
            .conn
            .dax_allocator()
            .ok_or(SystemError::EOPNOTSUPP_OR_ENOTSUP)?;
        let mut tree = self.dax_mappings.write();
        if !tree
            .tombstones
            .get(&tombstone.file_offset)
            .is_some_and(|mapping| Arc::ptr_eq(mapping, &tombstone.mapping))
        {
            return Err(SystemError::EINVAL);
        }
        let restored = allocator.cancel_reclaim(&tombstone.token)?;
        if !restored.same_identity(&tombstone.mapping.token) {
            return Err(SystemError::EINVAL);
        }
        tree.tombstones.remove(&tombstone.file_offset);
        tree.mappings
            .insert(tombstone.file_offset, tombstone.mapping);
        tree.bump_sequence()
    }

    pub(crate) fn dax_finish_reclaim(
        &self,
        tombstone: DaxReclaimTombstone,
    ) -> Result<(), SystemError> {
        self.dax_finish_reclaim_with_restore(tombstone, true)
    }

    fn dax_finish_reclaim_with_restore(
        &self,
        tombstone: DaxReclaimTombstone,
        restore_on_failure: bool,
    ) -> Result<(), SystemError> {
        let result = self.conn.remove_dax_mappings(
            self.dax_mapping_owner(),
            core::slice::from_ref(&tombstone.token),
        );
        let allocator = self
            .conn
            .dax_allocator()
            .ok_or(SystemError::EOPNOTSUPP_OR_ENOTSUP)?;
        let restore =
            restore_on_failure && result.is_err() && allocator.is_owned(&tombstone.mapping.token);
        let mut tree = self.dax_mappings.write();
        if tree
            .tombstones
            .get(&tombstone.file_offset)
            .is_some_and(|mapping| Arc::ptr_eq(mapping, &tombstone.mapping))
        {
            if result.is_ok() || restore_on_failure {
                tree.tombstones.remove(&tombstone.file_offset);
            }
            if restore {
                tree.mappings
                    .insert(tombstone.file_offset, tombstone.mapping);
            }
            if result.is_ok() || restore_on_failure {
                tree.bump_sequence()?;
            }
        }
        result
    }

    pub(crate) fn dax_mapping_for_offset(&self, offset: u64) -> Option<Arc<DaxMapping>> {
        self.dax_mappings
            .read()
            .mappings
            .get(&Self::dax_aligned_offset(offset))
            .cloned()
    }

    pub(crate) fn dax_file_size(&self) -> Result<usize, SystemError> {
        usize::try_from(self.cached_or_fetch_metadata()?.size.max(0))
            .map_err(|_| SystemError::EOVERFLOW)
    }

    /// Reclaim one daemon mapping after first revoking every PTE that can
    /// reference its 2 MiB cache-window range.  The epoch closes the race
    /// between the rmap walk and taking the inode layout lock.
    pub(crate) fn dax_reclaim_candidate(
        &self,
        candidate: &ReclaimCandidate,
    ) -> Result<(), SystemError> {
        let _reclaim_serial = self.dax_reclaim_serial.lock();
        let mapping = self
            .dax_mappings
            .read()
            .mappings
            .values()
            .find(|mapping| mapping.token.matches_candidate(candidate))
            .cloned()
            .ok_or(SystemError::EAGAIN_OR_EWOULDBLOCK)?;
        let expected_epoch = self.dax_pte_epoch();
        let page_cache = self.cached_page_cache();
        let start = usize::try_from(mapping.file_offset).map_err(|_| SystemError::EOVERFLOW)?
            >> crate::arch::MMArch::PAGE_SHIFT;
        let end = start
            .checked_add(DAX_RANGE_SIZE >> crate::arch::MMArch::PAGE_SHIFT)
            .ok_or(SystemError::EOVERFLOW)?;
        if let Some(cache) = page_cache.as_ref() {
            cache.unmap_mapping_pages(start, Some(end))?;
        }
        let tombstone = self.dax_isolate_reclaim(candidate, expected_epoch)?;
        if let Some(cache) = page_cache.as_ref() {
            if let Err(error) = cache.unmap_mapping_pages(start, Some(end)) {
                let _ = self.dax_cancel_reclaim(tombstone);
                return Err(error);
            }
        }
        self.dax_finish_reclaim(tombstone)
    }

    pub(crate) fn dax_read(&self, offset: usize, buf: &mut [u8]) -> Result<usize, SystemError> {
        if buf.is_empty() {
            return Ok(0);
        }
        let _admission = self.conn.enter_dax()?;
        let _layout = self.dax_layout_read();
        let size = self.cached_or_fetch_metadata()?.size.max(0) as usize;
        if offset >= size {
            return Ok(0);
        }
        let requested = core::cmp::min(buf.len(), size - offset);
        let mut done = 0usize;
        while done < requested {
            let current = offset.checked_add(done).ok_or(SystemError::EOVERFLOW)?;
            let in_mapping = current & (DAX_RANGE_SIZE - 1);
            let chunk = core::cmp::min(requested - done, DAX_RANGE_SIZE - in_mapping);
            let access = match self.dax_access_inner(current as u64, false, None) {
                Ok(access) => access,
                Err(error) if done == 0 => return Err(error),
                Err(_) => return Ok(done),
            };
            let _host_access = match self.dax_try_host_access() {
                Ok(guard) => guard,
                Err(error) if done == 0 => return Err(error),
                Err(_) => return Ok(done),
            };
            if let Err(error) = access.copy_to(in_mapping, &mut buf[done..done + chunk]) {
                return if done == 0 { Err(error) } else { Ok(done) };
            }
            done += chunk;
        }
        Ok(done)
    }

    /// Send ordinary FUSE WRITE while the caller owns the inode layout write lock.
    /// This helper deliberately does not acquire `writeback_barrier` again.
    pub(crate) fn fuse_write_locked(
        &self,
        offset: usize,
        buf: &[u8],
        private: &FuseOpenPrivateData,
        lock_owner: u64,
    ) -> Result<usize, SystemError> {
        let max_write = self.conn.max_write();
        if max_write == 0 {
            return Err(SystemError::EIO);
        }
        let mut done = 0usize;
        while done < buf.len() {
            let chunk = core::cmp::min(max_write, buf.len() - done);
            let current = offset.checked_add(done).ok_or(SystemError::EOVERFLOW)?;
            if current > i64::MAX as usize || chunk > u32::MAX as usize {
                return if done == 0 {
                    Err(SystemError::EFBIG)
                } else {
                    Ok(done)
                };
            }
            let input = FuseWriteIn {
                fh: private.fh,
                offset: current as u64,
                size: chunk as u32,
                write_flags: if lock_owner != 0 {
                    FUSE_WRITE_LOCKOWNER
                } else {
                    0
                },
                lock_owner,
                flags: private.open_flags,
                padding: 0,
            };
            let payload_len = core::mem::size_of::<FuseWriteIn>()
                .checked_add(chunk)
                .ok_or(SystemError::EOVERFLOW)?;
            let mut request = Vec::new();
            request
                .try_reserve_exact(payload_len)
                .map_err(|_| SystemError::ENOMEM)?;
            request.extend_from_slice(fuse_pack_struct(&input));
            request.extend_from_slice(&buf[done..done + chunk]);
            let payload = match self.conn.request(FUSE_WRITE, self.nodeid, &request) {
                Ok(payload) => payload,
                Err(error) if done == 0 => return Err(error),
                Err(_) => return Ok(done),
            };
            let output: FuseWriteOut = fuse_read_struct(&payload)?;
            let wrote = output.size as usize;
            if wrote > chunk {
                return if done == 0 {
                    Err(SystemError::EIO)
                } else {
                    Ok(done)
                };
            }
            self.note_successful_write(current, wrote)?;
            done += wrote;
            if wrote < chunk {
                break;
            }
        }
        Ok(done)
    }

    pub(crate) fn dax_write_or_fuse_locked(
        &self,
        offset: usize,
        buf: &[u8],
        private: &FuseOpenPrivateData,
        lock_owner: u64,
    ) -> Result<usize, SystemError> {
        if buf.is_empty() {
            return Ok(0);
        }
        let _admission = self.conn.enter_dax()?;
        let _layout = self.dax_layout_write();
        let end = offset
            .checked_add(buf.len())
            .ok_or(SystemError::EOVERFLOW)?;
        let size = self.cached_or_fetch_metadata()?.size.max(0) as usize;
        if offset >= size || end > size {
            return self.fuse_write_locked(offset, buf, private, lock_owner);
        }

        let mut done = 0usize;
        while done < buf.len() {
            let current = offset.checked_add(done).ok_or(SystemError::EOVERFLOW)?;
            let in_mapping = current & (DAX_RANGE_SIZE - 1);
            let chunk = core::cmp::min(buf.len() - done, DAX_RANGE_SIZE - in_mapping);
            let access = match self.dax_access_inner(current as u64, true, None) {
                Ok(access) => access,
                Err(error) if done == 0 => return Err(error),
                Err(_) => return Ok(done),
            };
            let _host_access = match self.dax_try_host_access() {
                Ok(guard) => guard,
                Err(error) if done == 0 => return Err(error),
                Err(_) => return Ok(done),
            };
            if let Err(error) = access.copy_from(in_mapping, &buf[done..done + chunk]) {
                return if done == 0 { Err(error) } else { Ok(done) };
            }
            self.note_successful_write(current, chunk)?;
            done += chunk;
        }
        Ok(done)
    }

    pub(crate) fn set_generation(&self, gen: u64) {
        self.generation.store(gen, Ordering::Relaxed);
    }

    pub(crate) fn mark_stale(&self) {
        self.stale.store(true, Ordering::Release);
    }

    pub(crate) fn check_not_stale(&self) -> Result<(), SystemError> {
        if self.stale.load(Ordering::Acquire) {
            return Err(SystemError::ESTALE);
        }
        Ok(())
    }

    pub fn nodeid(&self) -> u64 {
        self.nodeid
    }

    pub(crate) fn set_dname(&self, name: &str) {
        *self.name.lock() = Some(name.to_string());
    }

    pub(crate) fn has_dname(&self, name: &str) -> bool {
        self.name.lock().as_deref() == Some(name)
    }

    pub(crate) fn clear_dname_if(&self, name: &str) {
        let mut dname = self.name.lock();
        if dname.as_deref() == Some(name) {
            *dname = None;
        }
    }

    pub fn set_parent_nodeid(&self, parent: u64) {
        *self.parent_nodeid.lock() = parent;
    }

    pub(crate) fn set_parent_if_absent(&self, parent: Option<Arc<FuseNode>>) {
        let Some(parent) = parent else {
            return;
        };
        if parent.nodeid() == self.nodeid {
            return;
        }
        let mut guard = self.parent.lock();
        if guard.is_none() {
            *guard = Some(parent);
        }
    }

    pub(crate) fn set_parent(&self, parent: Option<Arc<FuseNode>>) {
        if parent
            .as_ref()
            .is_some_and(|parent| parent.nodeid() == self.nodeid)
        {
            return;
        }
        *self.parent.lock() = parent;
    }

    pub(crate) fn clear_parent(&self) {
        *self.parent.lock() = None;
    }

    pub(crate) fn cached_file_type(&self) -> Option<FileType> {
        self.cached_metadata.lock().as_ref().map(|md| md.file_type)
    }

    pub fn set_cached_metadata_with_valid(
        &self,
        md: Metadata,
        valid: u64,
        valid_nsec: u32,
        attr_flags: u32,
    ) {
        let md = self.metadata_with_dax_state(md);
        let mut metadata = self.cached_metadata.lock();
        *metadata = Some(md);
        self.bump_attr_version();
        drop(metadata);
        self.cached_metadata_deadline_ticks
            .store(Self::cache_deadline(valid, valid_nsec), Ordering::Relaxed);
        self.note_dax_attr_change(attr_flags);
    }

    /// Install an unsolicited/lookup/getattr daemon attribute snapshot.
    /// With writeback cache negotiated, locally maintained size and cmtime are
    /// authoritative and must not be rolled back by a stale daemon reply.
    pub(crate) fn merge_cached_metadata_from_daemon(
        &self,
        mut md: Metadata,
        valid: u64,
        valid_nsec: u32,
        request_epoch: u64,
        attr_flags: u32,
    ) -> Metadata {
        md = self.metadata_with_dax_state(md);
        let mut metadata = self.cached_metadata.lock();
        let stale_reply = self.attr_version() > request_epoch;
        if stale_reply {
            if let Some(local) = metadata.as_ref() {
                md = local.clone();
            }
        }
        if self
            .conn
            .has_init_flag(super::protocol::FUSE_WRITEBACK_CACHE)
            && md.file_type == FileType::File
        {
            if let Some(local) = metadata.as_ref() {
                md.size = local.size;
                md.mtime = local.mtime;
                md.ctime = local.ctime;
            }
        }
        *metadata = Some(md.clone());
        self.bump_attr_version();
        drop(metadata);
        self.cached_metadata_deadline_ticks.store(
            if stale_reply {
                0
            } else {
                Self::cache_deadline(valid, valid_nsec)
            },
            Ordering::Relaxed,
        );
        if !stale_reply {
            self.note_dax_attr_change(attr_flags);
        }
        md
    }

    pub(crate) fn attr_version(&self) -> u64 {
        self.attr_version.load(Ordering::Acquire)
    }

    pub(crate) fn bump_attr_version(&self) -> u64 {
        let version = self.conn.next_attr_epoch();
        self.attr_version.store(version, Ordering::Release);
        if version == 0 {
            self.short_read_source_attr_version
                .store(u64::MAX, Ordering::Release);
            self.short_read_chain_attr_version
                .store(u64::MAX, Ordering::Release);
        }
        version
    }

    pub(crate) fn invalidate_cached_metadata(&self) {
        self.bump_attr_version();
        self.cached_metadata_deadline_ticks
            .store(0, Ordering::Release);
    }

    /// 累计该 inode 在 userspace daemon 侧持有的 LOOKUP 引用。
    ///
    /// 对齐 Linux：每个成功的 LOOKUP/READDIRPLUS entry 都必须被记账，并在 inode
    /// 释放或卸载时用对应的 `FUSE_FORGET(nlookup=...)` 归还。打开的文件句柄会在
    /// `FuseOpenPrivateData` 中持有 `Arc<FuseNode>`，避免 fd 存活期间过早 FORGET。
    pub fn inc_lookup(&self, count: u64) {
        if self.nodeid == FUSE_ROOT_ID || count == 0 {
            return;
        }
        self.lookup_count.fetch_add(count, Ordering::Relaxed);
    }

    pub fn flush_forget(&self) {
        if self.nodeid == FUSE_ROOT_ID {
            return;
        }
        let nlookup = self.lookup_count.swap(0, Ordering::Relaxed);
        if nlookup == 0 {
            return;
        }
        let _ = self.conn.queue_forget(self.nodeid, nlookup);
    }

    fn now_ticks() -> u64 {
        // FUSE cache expiry is an elapsed-time deadline, not wall-clock time.
        // Match Linux fuse_time_to_jiffies(): use the monotonic timer tick
        // counter so each hot-path attr/entry-cache check is an in-memory read
        // and settimeofday cannot extend or prematurely expire the cache.
        clock()
    }

    fn cache_timeout_ticks(valid: u64, valid_nsec: u32) -> u64 {
        if valid == 0 && valid_nsec == 0 {
            return 0;
        }

        // Linux clamps the daemon-provided nanosecond component before
        // converting the relative timeout to jiffies.  Calculate in u128 so a
        // malformed, extremely large finite timeout cannot overflow into the
        // u64::MAX sentinel reserved for kernel-owned permanent snapshots.
        let nsec = valid_nsec.min(NSEC_PER_SEC - 1) as u128;
        let delta_ns = (valid as u128)
            .saturating_mul(NSEC_PER_SEC as u128)
            .saturating_add(nsec);
        delta_ns
            .div_ceil(NSEC_PER_JIFFY as u128)
            .min((u64::MAX - 1) as u128) as u64
    }

    fn cache_deadline(valid: u64, valid_nsec: u32) -> u64 {
        let delta_ticks = Self::cache_timeout_ticks(valid, valid_nsec);
        if delta_ticks == 0 {
            return 0;
        }
        Self::now_ticks()
            .checked_add(delta_ticks)
            .filter(|deadline| *deadline < u64::MAX)
            .unwrap_or(u64::MAX - 1)
    }

    pub(crate) fn conn(&self) -> &Arc<FuseConn> {
        &self.conn
    }

    pub(crate) fn fuse_fs(&self) -> Option<Arc<FuseFS>> {
        self.fs.upgrade()
    }

    pub(crate) fn parent_fuse_nodeid(&self) -> u64 {
        *self.parent_nodeid.lock()
    }

    fn request_name(&self, opcode: u32, nodeid: u64, name: &str) -> Result<FuseReply, SystemError> {
        self.check_not_stale()?;
        let payload = Self::pack_name_payload(name);
        self.conn().request(opcode, nodeid, &payload)
    }

    fn pack_name_payload(name: &str) -> Vec<u8> {
        let mut payload = Vec::with_capacity(name.len() + 1);
        payload.extend_from_slice(name.as_bytes());
        payload.push(0);
        payload
    }

    fn pack_struct_and_name_payload<T: Copy>(inarg: &T, name: &str) -> Vec<u8> {
        let mut payload = Vec::with_capacity(size_of::<T>() + name.len() + 1);
        payload.extend_from_slice(fuse_pack_struct(inarg));
        payload.extend_from_slice(name.as_bytes());
        payload.push(0);
        payload
    }

    fn fuse_xattr_unsupported(&self, opcode: u32) -> SystemError {
        self.conn.mark_no_xattr(opcode);
        SystemError::EOPNOTSUPP_OR_ENOTSUP
    }

    fn verify_xattr_list(list: &[u8]) -> Result<(), SystemError> {
        let mut idx = 0usize;
        while idx < list.len() {
            let Some(end) = list[idx..].iter().position(|b| *b == 0) else {
                return Err(SystemError::EIO);
            };
            if end == 0 {
                return Err(SystemError::EIO);
            }
            idx += end + 1;
        }
        Ok(())
    }

    fn pack_two_names_payload(first: &str, second: &str) -> Vec<u8> {
        let mut payload = Vec::with_capacity(first.len() + second.len() + 2);
        payload.extend_from_slice(first.as_bytes());
        payload.push(0);
        payload.extend_from_slice(second.as_bytes());
        payload.push(0);
        payload
    }

    fn entry_file_type(attr: &FuseAttr) -> Result<FileType, SystemError> {
        let mode = InodeMode::from_bits_truncate(attr.mode);
        match mode & InodeMode::S_IFMT {
            t if t == InodeMode::S_IFDIR => Ok(FileType::Dir),
            t if t == InodeMode::S_IFREG => Ok(FileType::File),
            t if t == InodeMode::S_IFLNK => Ok(FileType::SymLink),
            t if t == InodeMode::S_IFCHR => Ok(FileType::CharDevice),
            t if t == InodeMode::S_IFBLK => Ok(FileType::BlockDevice),
            t if t == InodeMode::S_IFSOCK => Ok(FileType::Socket),
            t if t == InodeMode::S_IFIFO => Ok(FileType::Pipe),
            _ => Err(SystemError::EIO),
        }
    }

    fn metadata_from_valid_entry(
        entry: &FuseEntryOut,
        zero_nodeid_error: SystemError,
        expected_type: Option<FileType>,
    ) -> Result<Metadata, SystemError> {
        if entry.nodeid == 0 {
            return Err(zero_nodeid_error);
        }
        if entry.attr.size > i64::MAX as u64 {
            return Err(SystemError::EIO);
        }
        let file_type = Self::entry_file_type(&entry.attr)?;
        if expected_type.is_some_and(|expected| expected != file_type) {
            return Err(SystemError::EIO);
        }
        Ok(Self::attr_to_metadata(&entry.attr))
    }

    fn attr_to_metadata(attr: &FuseAttr) -> Metadata {
        let mode = InodeMode::from_bits_truncate(attr.mode);
        let file_type = Self::entry_file_type(attr).unwrap_or(FileType::File);

        let inode_id = InodeId::new(attr.ino as usize);

        Metadata {
            dev_id: 0,
            inode_id,
            size: attr.size as i64,
            blk_size: attr.blksize as usize,
            blocks: attr.blocks as usize,
            atime: PosixTimeSpec::new(attr.atime as i64, attr.atimensec as i64),
            mtime: PosixTimeSpec::new(attr.mtime as i64, attr.mtimensec as i64),
            ctime: PosixTimeSpec::new(attr.ctime as i64, attr.ctimensec as i64),
            btime: PosixTimeSpec::default(),
            file_type,
            mode,
            flags: InodeFlags::empty(),
            nlinks: attr.nlink as usize,
            uid: attr.uid as usize,
            gid: attr.gid as usize,
            raw_dev: DeviceNumber::default(),
        }
    }

    fn fetch_attr_with_file_handle(&self, fh: Option<u64>) -> Result<Metadata, SystemError> {
        self.check_not_stale()?;
        let request_epoch = self.conn.sample_attr_epoch();
        let getattr_in = getattr_request_input(fh);
        let payload =
            self.conn()
                .request(FUSE_GETATTR, self.nodeid, fuse_pack_struct(&getattr_in))?;
        let out: FuseAttrOut = fuse_read_struct(&payload)?;
        let md = Self::attr_to_metadata(&out.attr);
        Ok(self.merge_cached_metadata_from_daemon(
            md,
            out.attr_valid,
            out.attr_valid_nsec,
            request_epoch,
            out.attr.flags,
        ))
    }

    fn fetch_attr(&self) -> Result<Metadata, SystemError> {
        self.fetch_attr_with_file_handle(None)
    }

    fn cached_or_fetch_metadata(&self) -> Result<Metadata, SystemError> {
        self.conn.check_allow_current_process()?;
        self.cached_or_fetch_metadata_with_file_handle(None)
    }

    fn cached_or_fetch_metadata_with_file_handle(
        &self,
        fh: Option<u64>,
    ) -> Result<Metadata, SystemError> {
        if let Some(m) = self.cached_metadata.lock().clone() {
            let deadline = self.cached_metadata_deadline_ticks.load(Ordering::Relaxed);
            if deadline == u64::MAX || (deadline != 0 && Self::now_ticks() < deadline) {
                return Ok(m);
            }
        }
        match fh {
            Some(fh) => self.fetch_attr_with_file_handle(Some(fh)),
            None => self.fetch_attr(),
        }
    }

    /// Refresh attributes for I/O issued through an already-open file.
    ///
    /// Mount permission was checked when the file was opened.  Linux's
    /// `fuse_update_attributes()` does not repeat that check for every cached
    /// read, but it still honors the attribute timeout and may issue GETATTR.
    pub(crate) fn update_cached_metadata_for_open_io(
        &self,
        cached: Metadata,
        fh: u64,
    ) -> Result<Metadata, SystemError> {
        // `read_at()` already took the inode metadata snapshot needed for the
        // cached EOF/type checks. Re-locking the sleeping metadata mutex here
        // for every AUTO_INVAL_DATA read dominates 4 KiB cached I/O. The TTL is
        // independently published, so reuse that snapshot while it remains
        // valid and issue GETATTR_FH only after expiry.
        let deadline = self.cached_metadata_deadline_ticks.load(Ordering::Acquire);
        if deadline == u64::MAX || (deadline != 0 && Self::now_ticks() < deadline) {
            return Ok(cached);
        }
        self.fetch_attr_with_file_handle(Some(fh))
    }
}

impl Drop for FuseNode {
    fn drop(&mut self) {
        if self.dax_active() {
            let revoke = if self.conn.dax_admission_state() == DaxAdmissionState::Active {
                match self.dax_teardown() {
                    Ok(()) => Ok(()),
                    Err(error) => {
                        log::warn!(
                            "fuse: inode {} dropped with DAX teardown error: {:?}",
                            self.nodeid,
                            error
                        );
                        // The daemon did not confirm removal. Revoke local
                        // PTEs/tree and retire the ranges before unregistering.
                        self.dax_abandon_mappings()
                    }
                }
            } else {
                self.dax_disconnect_revoke()
            };
            if let Err(ref error) = revoke {
                log::warn!(
                    "fuse: inode {} dropped with local DAX revoke error: {:?}",
                    self.nodeid,
                    error
                );
            }
            if self.conn.dax_admission_state() != DaxAdmissionState::Active || revoke.is_err() {
                self.conn
                    .finish_dax_node_drop(self.dax_mapping_owner(), revoke);
            } else {
                self.conn.unregister_dax_node(self.dax_mapping_owner());
            }
        }
        self.clear_lookup_cache_tree();
        self.flush_forget();
        self.clear_parent();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn getattr_request_uses_fh_only_for_open_file_queries() {
        let path_query = getattr_request_input(None);
        assert_eq!(path_query.getattr_flags, 0);
        assert_eq!(path_query.fh, 0);

        let open_file_query = getattr_request_input(Some(0x1234_5678));
        assert_eq!(open_file_query.getattr_flags, FUSE_GETATTR_FH);
        assert_eq!(open_file_query.fh, 0x1234_5678);
    }

    #[test]
    fn cache_timeout_matches_linux_jiffy_and_nsec_rules() {
        assert_eq!(FuseNode::cache_timeout_ticks(0, 0), 0);
        assert_eq!(FuseNode::cache_timeout_ticks(0, 1), 1);

        let clamped = FuseNode::cache_timeout_ticks(0, NSEC_PER_SEC - 1);
        assert_eq!(FuseNode::cache_timeout_ticks(0, u32::MAX), clamped);
        assert_eq!(
            FuseNode::cache_timeout_ticks(1, 0),
            (NSEC_PER_SEC as u64).div_ceil(NSEC_PER_JIFFY as u64)
        );
    }

    #[test]
    fn finite_cache_timeout_never_becomes_permanent_sentinel() {
        assert_eq!(
            FuseNode::cache_timeout_ticks(u64::MAX, u32::MAX),
            u64::MAX - 1
        );
        assert_ne!(FuseNode::cache_deadline(u64::MAX, u32::MAX), u64::MAX);
    }

    #[test]
    fn dax_host_invalidation_gate_blocks_until_last_notification_finishes() {
        let gate = DaxHostInvalidationGate::new();
        let access = gate.try_enter().unwrap();
        assert_eq!(gate.active.load(Ordering::Acquire), 1);
        drop(access);

        let first = gate.begin().unwrap();
        let second = gate.begin().unwrap();
        assert!(gate.blocked());
        assert!(matches!(
            gate.try_enter(),
            Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
        ));
        drop(first);
        assert!(gate.blocked());
        drop(second);
        assert!(!gate.blocked());

        let access = gate.try_enter().unwrap();
        assert_eq!(gate.active.load(Ordering::Acquire), 1);
        drop(access);
        assert_eq!(gate.active.load(Ordering::Acquire), 0);
    }
}
