use alloc::{sync::Arc, vec::Vec};

use hashbrown::HashMap;
use jhash::jhash2;

use crate::libs::lazy_init::Lazy;
use crate::libs::mutex::Mutex;

use super::InodeId;

/// Append lock key: uniquely identifies an inode across filesystems.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct AppendLockKey {
    /// Device ID
    dev_id: usize,
    /// Inode ID
    inode_id: InodeId,
}

/// 这里51个bucket是因为，AppendLockKey的大小加起来刚好小于2048字节，
/// 内核会分配2K给这个结构体，所以51个bucket刚好合适，避免内部碎片
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
        let dev_id = key.dev_id as u64;
        let inode_id = key.inode_id.data() as u64;
        let key_array = [
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
    /// - Map access is protected by a sharded spinlock to avoid a single global bottleneck.
    /// - The per-inode lock is a sleeping `Mutex` since the critical section may schedule.
    /// - We opportunistically remove the map entry when it becomes unused.
    pub fn with_lock<R>(&self, dev_id: usize, inode_id: InodeId, f: impl FnOnce() -> R) -> R {
        let key = AppendLockKey { dev_id, inode_id };
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
pub fn with_inode_append_lock<R>(dev_id: usize, inode_id: InodeId, f: impl FnOnce() -> R) -> R {
    APPEND_LOCK_MANAGER.get().with_lock(dev_id, inode_id, f)
}
