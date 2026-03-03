use alloc::{sync::Arc, vec::Vec};

use hashbrown::{HashMap, HashSet};
use jhash::jhash2;
use system_error::SystemError;

use crate::libs::{casting::DowncastArc, lazy_init::Lazy, mutex::Mutex, wait_queue::WaitQueue};

use super::{file::File, mount::MountFSInode, IndexNode, InodeId};

const FLOCK_SHARDS: usize = 53;
type OwnerId = usize;

#[derive(Clone, Copy, Eq, PartialEq, Hash)]
struct FlockKey {
    dev_id: usize,
    inode_id: InodeId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FlockOperation {
    Shared,
    Exclusive,
    Unlock,
}

#[derive(Default)]
struct FlockEntryState {
    exclusive_owner: Option<OwnerId>,
    shared_owners: HashSet<OwnerId>,
}

impl FlockEntryState {
    #[inline]
    fn owner_lock(&self, owner: OwnerId) -> Option<FlockOperation> {
        if self.exclusive_owner == Some(owner) {
            Some(FlockOperation::Exclusive)
        } else if self.shared_owners.contains(&owner) {
            Some(FlockOperation::Shared)
        } else {
            None
        }
    }

    #[inline]
    fn remove_owner(&mut self, owner: OwnerId) -> bool {
        let mut changed = false;
        if self.exclusive_owner == Some(owner) {
            self.exclusive_owner = None;
            changed = true;
        }
        if self.shared_owners.remove(&owner) {
            changed = true;
        }
        changed
    }

    #[inline]
    fn has_conflict(&self, owner: OwnerId, req: FlockOperation) -> bool {
        match req {
            FlockOperation::Shared => self
                .exclusive_owner
                .is_some_and(|exclusive_owner| exclusive_owner != owner),
            FlockOperation::Exclusive => {
                if self
                    .exclusive_owner
                    .is_some_and(|exclusive_owner| exclusive_owner != owner)
                {
                    return true;
                }
                self.shared_owners
                    .iter()
                    .any(|shared_owner| *shared_owner != owner)
            }
            FlockOperation::Unlock => false,
        }
    }

    #[inline]
    fn acquire(&mut self, owner: OwnerId, req: FlockOperation) {
        match req {
            FlockOperation::Shared => {
                debug_assert!(self.exclusive_owner.is_none());
                self.shared_owners.insert(owner);
            }
            FlockOperation::Exclusive => {
                debug_assert!(self.exclusive_owner.is_none());
                debug_assert!(self.shared_owners.is_empty());
                self.exclusive_owner = Some(owner);
            }
            FlockOperation::Unlock => {}
        }
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.exclusive_owner.is_none() && self.shared_owners.is_empty()
    }
}

struct FlockEntry {
    state: Mutex<FlockEntryState>,
    waitq: WaitQueue,
}

impl FlockEntry {
    #[inline]
    fn new() -> Self {
        Self {
            state: Mutex::new(FlockEntryState::default()),
            waitq: WaitQueue::default(),
        }
    }

    #[inline]
    fn unlock_owner(&self, owner: OwnerId) -> bool {
        self.state.lock().remove_owner(owner)
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.state.lock().is_empty()
    }
}

struct FlockShard {
    map: Mutex<HashMap<FlockKey, Arc<FlockEntry>>>,
}

pub struct FlockManager {
    shards: Vec<FlockShard>,
}

impl FlockManager {
    pub fn new() -> Self {
        let mut shards = Vec::with_capacity(FLOCK_SHARDS);
        for _ in 0..FLOCK_SHARDS {
            shards.push(FlockShard {
                map: Mutex::new(HashMap::new()),
            });
        }
        Self { shards }
    }

    #[inline]
    fn shard_index(key: &FlockKey) -> usize {
        let dev_id = key.dev_id as u64;
        let inode_id = key.inode_id.data() as u64;
        let key_array = [
            (dev_id >> 32) as u32,
            dev_id as u32,
            (inode_id >> 32) as u32,
            inode_id as u32,
        ];
        let hash = jhash2(&key_array, 0);
        (hash as usize) % FLOCK_SHARDS
    }

    #[inline]
    fn shard(&self, key: &FlockKey) -> &FlockShard {
        &self.shards[Self::shard_index(key)]
    }

    fn canonical_inode_for_lock(file: &File) -> Arc<dyn IndexNode> {
        // 对 flock key 计算，统一剥离 MountFSInode 包装，避免 mount 侧
        // metadata.dev_id 合成策略导致同一底层 inode 被误判为不同锁对象。
        let mut inode = file.inode();
        loop {
            match inode.clone().downcast_arc::<MountFSInode>() {
                Some(mnt_inode) => inode = mnt_inode.underlying_inode(),
                None => return inode,
            }
        }
    }

    fn key_from_file(file: &File) -> Result<FlockKey, SystemError> {
        let inode = Self::canonical_inode_for_lock(file);
        let md = inode.metadata()?;
        Ok(FlockKey {
            dev_id: md.dev_id,
            inode_id: md.inode_id,
        })
    }

    fn get_or_create_entry(&self, key: FlockKey) -> Arc<FlockEntry> {
        let shard = self.shard(&key);
        let mut guard = shard.map.lock();
        guard
            .entry(key)
            .or_insert_with(|| Arc::new(FlockEntry::new()))
            .clone()
    }

    fn get_entry(&self, key: &FlockKey) -> Option<Arc<FlockEntry>> {
        let shard = self.shard(key);
        shard.map.lock().get(key).cloned()
    }

    fn lock_or_wait(
        entry: &Arc<FlockEntry>,
        owner: OwnerId,
        req: FlockOperation,
        nonblocking: bool,
    ) -> Result<(), SystemError> {
        debug_assert!(matches!(
            req,
            FlockOperation::Shared | FlockOperation::Exclusive
        ));

        let mut dropped_old_lock = false;
        {
            let mut state = entry.state.lock();
            if let Some(current_lock) = state.owner_lock(owner) {
                if current_lock == req {
                    return Ok(());
                }
                let _ = state.remove_owner(owner);
                dropped_old_lock = true;
            }

            if !state.has_conflict(owner, req) {
                state.acquire(owner, req);
                drop(state);
                if dropped_old_lock {
                    entry.waitq.wakeup_all(None);
                }
                return Ok(());
            }
        }

        if dropped_old_lock {
            entry.waitq.wakeup_all(None);
        }

        if nonblocking {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }

        entry.waitq.wait_until_interruptible(|| {
            let mut state = entry.state.lock();
            if state.has_conflict(owner, req) {
                None
            } else {
                state.acquire(owner, req);
                Some(())
            }
        })?;

        Ok(())
    }

    fn try_cleanup_entry(&self, key: &FlockKey, entry: &Arc<FlockEntry>) {
        if !entry.is_empty() || !entry.waitq.is_empty() {
            return;
        }

        let shard = self.shard(key);
        let mut guard = shard.map.lock();
        if let Some(current) = guard.get(key) {
            if Arc::ptr_eq(current, entry)
                && entry.is_empty()
                && entry.waitq.is_empty()
                && Arc::strong_count(entry) == 2
            {
                guard.remove(key);
            }
        }
    }

    pub fn apply(
        &self,
        file: &Arc<File>,
        operation: FlockOperation,
        nonblocking: bool,
    ) -> Result<(), SystemError> {
        let key = Self::key_from_file(file.as_ref())?;
        let owner = file.open_file_id();
        let entry = self.get_or_create_entry(key);

        let result = match operation {
            FlockOperation::Unlock => {
                if entry.unlock_owner(owner) {
                    entry.waitq.wakeup_all(None);
                }
                Ok(())
            }
            FlockOperation::Shared | FlockOperation::Exclusive => {
                Self::lock_or_wait(&entry, owner, operation, nonblocking)
            }
        };

        self.try_cleanup_entry(&key, &entry);
        result
    }

    pub fn release_file(&self, file: &File) {
        let Ok(key) = Self::key_from_file(file) else {
            return;
        };
        let owner = file.open_file_id();
        let Some(entry) = self.get_entry(&key) else {
            return;
        };

        if entry.unlock_owner(owner) {
            entry.waitq.wakeup_all(None);
        }
        self.try_cleanup_entry(&key, &entry);
    }
}

static FLOCK_MANAGER: Lazy<FlockManager> = Lazy::new();

pub fn init_flock_manager() {
    if !FLOCK_MANAGER.initialized() {
        FLOCK_MANAGER.init(FlockManager::new());
    }
}

#[inline]
pub fn apply_flock(
    file: &Arc<File>,
    operation: FlockOperation,
    nonblocking: bool,
) -> Result<(), SystemError> {
    FLOCK_MANAGER.get().apply(file, operation, nonblocking)
}

pub fn release_all_for_file(file: &File) {
    if !FLOCK_MANAGER.initialized() {
        return;
    }
    FLOCK_MANAGER.get().release_file(file);
}
