use alloc::{sync::Arc, vec::Vec};

use hashbrown::HashMap;
use jhash::jhash2;

use crate::libs::lazy_init::Lazy;
use crate::libs::mutex::Mutex;

use super::InodeId;

/// Internal append-lock identity for one inode in one filesystem instance.
///
/// `dev_id`/`inode_id` alone are not sufficient: stacked filesystems such as
/// overlayfs may deliberately expose the backing inode's stat identity while
/// still owning a distinct inode lock domain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct AppendLockKey {
    /// Allocation identity of the canonical filesystem instance.
    fs_instance: usize,
    /// Device namespace within that filesystem instance.
    dev_id: usize,
    /// Inode identity within the device namespace.
    inode_id: InodeId,
}

impl AppendLockKey {
    pub(super) const fn new(fs_instance: usize, dev_id: usize, inode_id: InodeId) -> Self {
        Self {
            fs_instance,
            dev_id,
            inode_id,
        }
    }
}

/// Keep enough shards to avoid a single append-lock map bottleneck without
/// allocating a large fixed table. Each shard owns its entries dynamically.
const APPEND_LOCK_SHARDS: usize = 51;

struct AppendLockShard {
    map: Mutex<HashMap<AppendLockKey, Arc<Mutex<()>>>>,
}

pub struct AppendLockManager {
    // Store shards on heap to keep the global manager small (avoid wasting a whole page).
    shards: Vec<AppendLockShard>,
}

impl AppendLockManager {
    pub fn new() -> Self {
        let mut shards = Vec::with_capacity(APPEND_LOCK_SHARDS);
        for _ in 0..APPEND_LOCK_SHARDS {
            shards.push(AppendLockShard {
                map: Mutex::new(HashMap::new()),
            });
        }
        Self { shards }
    }

    #[inline]
    fn shard_index(key: &AppendLockKey) -> usize {
        // Use jhash to compute a stable hash for sharding.
        // Convert usize values to u32 arrays for jhash2.
        let fs_instance = key.fs_instance as u64;
        let dev_id = key.dev_id as u64;
        let inode_id = key.inode_id.data() as u64;
        let key_array = [
            (fs_instance >> 32) as u32,
            fs_instance as u32,
            (dev_id >> 32) as u32,
            dev_id as u32,
            (inode_id >> 32) as u32,
            inode_id as u32,
        ];
        let hash = jhash2(&key_array, 0);
        (hash as usize) % APPEND_LOCK_SHARDS
    }

    /// Run `f` while holding the per-inode append lock.
    ///
    /// Notes:
    /// - Map access is protected by sharded mutexes to avoid a single global bottleneck.
    /// - The per-inode lock is a sleeping `Mutex` since the critical section may schedule.
    /// - We opportunistically remove the map entry when it becomes unused.
    fn with_lock<R>(&self, key: AppendLockKey, f: impl FnOnce() -> R) -> R {
        let shard_idx = Self::shard_index(&key);
        let shard = &self.shards[shard_idx];

        // 1) Get or create the per-inode mutex (short spin-locked section).
        let lock_arc: Arc<Mutex<()>> = {
            let mut guard = shard.map.lock();
            guard
                .entry(key)
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };

        // 2) Hold the inode lock for the duration of the append-critical section.
        let _inode_guard = lock_arc.lock();
        let ret = f();
        drop(_inode_guard);

        // 3) Opportunistic cleanup: if only the map and this local variable hold the Arc,
        // remove it from the shard map to avoid unbounded growth.
        {
            let mut guard = shard.map.lock();
            if let Some(current) = guard.get(&key) {
                if Arc::ptr_eq(current, &lock_arc) && Arc::strong_count(&lock_arc) == 2 {
                    guard.remove(&key);
                }
            }
        }

        ret
    }
}

static APPEND_LOCK_MANAGER: Lazy<AppendLockManager> = Lazy::new();

/// Initialize the global append lock manager.
///
/// Must be called during VFS init before any file write path uses append locks.
pub fn init_append_lock_manager() {
    if !APPEND_LOCK_MANAGER.initialized() {
        APPEND_LOCK_MANAGER.init(AppendLockManager::new());
    }
}

#[inline]
pub(super) fn with_inode_append_lock<R>(key: AppendLockKey, f: impl FnOnce() -> R) -> R {
    APPEND_LOCK_MANAGER.get().with_lock(key, f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_lock_key_keeps_stacked_filesystem_domains_distinct() {
        let inode = InodeId::new(42);
        let overlay = AppendLockKey::new(0x1000, 7, inode);
        let backing = AppendLockKey::new(0x2000, 7, inode);

        assert_ne!(overlay, backing);
    }

    #[test]
    fn append_lock_key_keeps_device_namespaces_distinct() {
        let inode = InodeId::new(42);
        let first_origin = AppendLockKey::new(0x1000, 7, inode);
        let second_origin = AppendLockKey::new(0x1000, 8, inode);

        assert_ne!(first_origin, second_origin);
    }

    #[test]
    fn append_lock_key_matches_same_canonical_inode() {
        let first = AppendLockKey::new(0x1000, 7, InodeId::new(42));
        let second = AppendLockKey::new(0x1000, 7, InodeId::new(42));

        assert_eq!(first, second);
    }
}
