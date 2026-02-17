use alloc::{sync::Arc, vec::Vec};

use hashbrown::HashMap;
use jhash::jhash2;
use system_error::SystemError;

use crate::libs::{lazy_init::Lazy, mutex::Mutex, wait_queue::WaitQueue};

use super::{
    fcntl::{PosixFlock, F_RDLCK, F_UNLCK, F_WRLCK},
    file::{File, FileMode},
    syscall::{SEEK_CUR, SEEK_END, SEEK_SET},
    InodeId,
};

const POSIX_LOCK_SHARDS: usize = 53;

#[derive(Clone, Copy, Eq, PartialEq, Hash)]
struct PosixLockKey {
    dev_id: usize,
    inode_id: InodeId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PosixLockType {
    Read,
    Write,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PosixLockRequestType {
    Read,
    Write,
    Unlock,
}

impl PosixLockRequestType {
    #[inline]
    fn from_user_type(v: i16) -> Option<Self> {
        match v {
            F_RDLCK => Some(Self::Read),
            F_WRLCK => Some(Self::Write),
            F_UNLCK => Some(Self::Unlock),
            _ => None,
        }
    }

    #[inline]
    fn as_lock_type(self) -> Option<PosixLockType> {
        match self {
            Self::Read => Some(PosixLockType::Read),
            Self::Write => Some(PosixLockType::Write),
            Self::Unlock => None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct ParsedRangeLock {
    req_type: PosixLockRequestType,
    start: i64,
    end: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PosixLockRecord {
    owner_id: usize,
    owner_pid: i32,
    lock_type: PosixLockType,
    start: i64,
    end: i64,
}

#[derive(Default)]
struct PosixLockEntryState {
    locks: Vec<PosixLockRecord>,
}

impl PosixLockEntryState {
    #[inline]
    fn is_empty(&self) -> bool {
        self.locks.is_empty()
    }

    #[inline]
    fn overlap(a_start: i64, a_end: i64, b_start: i64, b_end: i64) -> bool {
        !(a_end < b_start || b_end < a_start)
    }

    #[inline]
    fn conflict(req: PosixLockType, existing: PosixLockType) -> bool {
        matches!(req, PosixLockType::Write) || matches!(existing, PosixLockType::Write)
    }

    fn first_conflict(
        &self,
        owner_id: usize,
        req: PosixLockType,
        start: i64,
        end: i64,
    ) -> Option<PosixLockRecord> {
        for rec in self.locks.iter().copied() {
            if rec.owner_id == owner_id {
                continue;
            }
            if !Self::overlap(start, end, rec.start, rec.end) {
                continue;
            }
            if Self::conflict(req, rec.lock_type) {
                return Some(rec);
            }
        }
        None
    }

    fn remove_all_for_owner(&mut self, owner_id: usize) -> bool {
        let old_len = self.locks.len();
        self.locks.retain(|rec| rec.owner_id != owner_id);
        old_len != self.locks.len()
    }

    fn carve_owner_range(&mut self, owner_id: usize, start: i64, end: i64) {
        let mut next = Vec::with_capacity(self.locks.len());
        for rec in self.locks.iter().copied() {
            if rec.owner_id != owner_id || !Self::overlap(start, end, rec.start, rec.end) {
                next.push(rec);
                continue;
            }

            if rec.start < start {
                next.push(PosixLockRecord {
                    end: start - 1,
                    ..rec
                });
            }

            if rec.end > end {
                next.push(PosixLockRecord {
                    start: end + 1,
                    ..rec
                });
            }
        }
        self.locks = next;
    }

    fn normalize(&mut self) {
        self.locks
            .sort_by_key(|rec| (rec.owner_id, rec.start, rec.end, rec.lock_type as u8));

        let mut merged: Vec<PosixLockRecord> = Vec::with_capacity(self.locks.len());
        for rec in self.locks.iter().copied() {
            if let Some(last) = merged.last_mut() {
                let adjacent_or_overlap =
                    last.end == i64::MAX || rec.start <= last.end.saturating_add(1);
                if last.owner_id == rec.owner_id
                    && last.lock_type == rec.lock_type
                    && adjacent_or_overlap
                {
                    if rec.end > last.end {
                        last.end = rec.end;
                    }
                    continue;
                }
            }
            merged.push(rec);
        }

        self.locks = merged;
    }

    fn apply_owner_request(
        &mut self,
        owner_id: usize,
        owner_pid: i32,
        req: ParsedRangeLock,
    ) -> bool {
        let old = self.locks.clone();

        self.carve_owner_range(owner_id, req.start, req.end);
        if let Some(lock_type) = req.req_type.as_lock_type() {
            self.locks.push(PosixLockRecord {
                owner_id,
                owner_pid,
                lock_type,
                start: req.start,
                end: req.end,
            });
        }

        self.normalize();
        self.locks != old
    }
}

struct PosixLockEntry {
    state: Mutex<PosixLockEntryState>,
    waitq: WaitQueue,
}

impl PosixLockEntry {
    #[inline]
    fn new() -> Self {
        Self {
            state: Mutex::new(PosixLockEntryState::default()),
            waitq: WaitQueue::default(),
        }
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.state.lock().is_empty()
    }
}

struct PosixLockShard {
    map: Mutex<HashMap<PosixLockKey, Arc<PosixLockEntry>>>,
}

#[derive(Default)]
struct WaitGraph {
    edges: HashMap<usize, HashMap<usize, usize>>,
}

impl WaitGraph {
    fn add_edge(&mut self, src_owner: usize, dst_owner: usize) {
        let dsts = self.edges.entry(src_owner).or_default();
        *dsts.entry(dst_owner).or_insert(0) += 1;
    }

    fn remove_edge(&mut self, src_owner: usize, dst_owner: usize) {
        let mut need_remove_src = false;
        if let Some(dsts) = self.edges.get_mut(&src_owner) {
            let mut need_remove_dst = false;
            if let Some(cnt) = dsts.get_mut(&dst_owner) {
                if *cnt <= 1 {
                    need_remove_dst = true;
                } else {
                    *cnt -= 1;
                }
            }
            if need_remove_dst {
                dsts.remove(&dst_owner);
            }
            if dsts.is_empty() {
                need_remove_src = true;
            }
        }
        if need_remove_src {
            self.edges.remove(&src_owner);
        }
    }

    fn has_path(&self, from_owner: usize, target_owner: usize) -> bool {
        if from_owner == target_owner {
            return true;
        }

        let mut stack = Vec::with_capacity(1);
        stack.push(from_owner);
        let mut visited = hashbrown::HashSet::new();

        while let Some(curr) = stack.pop() {
            if !visited.insert(curr) {
                continue;
            }
            let Some(nexts) = self.edges.get(&curr) else {
                continue;
            };
            for (&next, &count) in nexts.iter() {
                if count == 0 {
                    continue;
                }
                if next == target_owner {
                    return true;
                }
                stack.push(next);
            }
        }
        false
    }
}

pub struct PosixLockManager {
    shards: Vec<PosixLockShard>,
    wait_graph: Mutex<WaitGraph>,
}

impl PosixLockManager {
    pub fn new() -> Self {
        let mut shards = Vec::with_capacity(POSIX_LOCK_SHARDS);
        for _ in 0..POSIX_LOCK_SHARDS {
            shards.push(PosixLockShard {
                map: Mutex::new(HashMap::new()),
            });
        }
        Self {
            shards,
            wait_graph: Mutex::new(WaitGraph::default()),
        }
    }

    #[inline]
    fn shard_index(key: &PosixLockKey) -> usize {
        let dev_id = key.dev_id as u64;
        let inode_id = key.inode_id.data() as u64;
        let key_array = [
            (dev_id >> 32) as u32,
            dev_id as u32,
            (inode_id >> 32) as u32,
            inode_id as u32,
        ];
        let hash = jhash2(&key_array, 0);
        (hash as usize) % POSIX_LOCK_SHARDS
    }

    #[inline]
    fn shard(&self, key: &PosixLockKey) -> &PosixLockShard {
        &self.shards[Self::shard_index(key)]
    }

    #[inline]
    fn key_from_file(file: &File) -> PosixLockKey {
        let (dev_id, inode_id) = file.posix_lock_key();
        PosixLockKey { dev_id, inode_id }
    }

    fn get_or_create_entry(&self, key: PosixLockKey) -> Arc<PosixLockEntry> {
        let shard = self.shard(&key);
        let mut guard = shard.map.lock();
        guard
            .entry(key)
            .or_insert_with(|| Arc::new(PosixLockEntry::new()))
            .clone()
    }

    fn get_entry(&self, key: &PosixLockKey) -> Option<Arc<PosixLockEntry>> {
        self.shard(key).map.lock().get(key).cloned()
    }

    fn try_cleanup_entry(&self, key: &PosixLockKey, entry: &Arc<PosixLockEntry>) {
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

    fn lock_or_wait(
        &self,
        entry: &Arc<PosixLockEntry>,
        owner_id: usize,
        owner_pid: i32,
        req: ParsedRangeLock,
        blocking: bool,
    ) -> Result<bool, SystemError> {
        let req_type = req
            .req_type
            .as_lock_type()
            .expect("lock_or_wait requires lock request");

        loop {
            let conflict_owner = {
                let mut state = entry.state.lock();
                if let Some(conflict) = state.first_conflict(owner_id, req_type, req.start, req.end)
                {
                    conflict.owner_id
                } else {
                    return Ok(state.apply_owner_request(owner_id, owner_pid, req));
                }
            };

            if !blocking {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }

            {
                let mut graph = self.wait_graph.lock();
                if graph.has_path(conflict_owner, owner_id) {
                    return Err(SystemError::EDEADLK_OR_EDEADLOCK);
                }
                graph.add_edge(owner_id, conflict_owner);
            }

            // Recheck once after edge insertion. If conflict already disappeared,
            // remove the edge immediately and retry acquisition without sleeping.
            {
                let state = entry.state.lock();
                let conflict_after_edge = state
                    .first_conflict(owner_id, req_type, req.start, req.end)
                    .map(|c| c.owner_id);
                drop(state);

                match conflict_after_edge {
                    None => {
                        self.wait_graph.lock().remove_edge(owner_id, conflict_owner);
                        continue;
                    }
                    Some(new_conflict_owner) if new_conflict_owner != conflict_owner => {
                        // Blocker changed before sleeping. Rebuild edge in next loop
                        // so deadlock detection is checked against the new blocker.
                        self.wait_graph.lock().remove_edge(owner_id, conflict_owner);
                        continue;
                    }
                    Some(_) => {}
                }
            }

            let wait_result = entry.waitq.wait_until_interruptible(|| {
                let state = entry.state.lock();
                match state.first_conflict(owner_id, req_type, req.start, req.end) {
                    None => Some(()),
                    Some(conflict) if conflict.owner_id != conflict_owner => {
                        // Current blocker changed; wake and re-evaluate wait graph edge.
                        Some(())
                    }
                    Some(_) => None,
                }
            });
            self.wait_graph.lock().remove_edge(owner_id, conflict_owner);
            wait_result?;
        }
    }

    fn check_fmode_for_setlk(
        file: &File,
        req_type: PosixLockRequestType,
    ) -> Result<(), SystemError> {
        let mode = file.mode();
        match req_type {
            PosixLockRequestType::Read => {
                if !mode.contains(FileMode::FMODE_READ) {
                    return Err(SystemError::EBADF);
                }
            }
            PosixLockRequestType::Write => {
                if !mode.contains(FileMode::FMODE_WRITE) {
                    return Err(SystemError::EBADF);
                }
            }
            PosixLockRequestType::Unlock => {}
        }
        Ok(())
    }

    pub fn get_lock(
        &self,
        file: &Arc<File>,
        owner_id: usize,
        flock: &mut PosixFlock,
    ) -> Result<(), SystemError> {
        let req = parse_flock_to_range(file.as_ref(), flock, false)?;
        let req_type = req.req_type.as_lock_type().ok_or(SystemError::EINVAL)?;
        let key = Self::key_from_file(file.as_ref());
        let Some(entry) = self.get_entry(&key) else {
            flock.l_type = F_UNLCK;
            return Ok(());
        };

        let conflict = {
            let state = entry.state.lock();
            state.first_conflict(owner_id, req_type, req.start, req.end)
        };

        if let Some(lock) = conflict {
            flock.l_type = match lock.lock_type {
                PosixLockType::Read => F_RDLCK,
                PosixLockType::Write => F_WRLCK,
            };
            flock.l_whence = SEEK_SET as i16;
            flock.l_start = lock.start;
            flock.l_len = if lock.end == i64::MAX {
                0
            } else {
                lock.end - lock.start + 1
            };
            flock.l_pid = lock.owner_pid;
        } else {
            flock.l_type = F_UNLCK;
        }

        Ok(())
    }

    pub fn set_lock(
        &self,
        file: &Arc<File>,
        owner_id: usize,
        owner_pid: i32,
        flock: &PosixFlock,
        blocking: bool,
    ) -> Result<(), SystemError> {
        let req = parse_flock_to_range(file.as_ref(), flock, true)?;
        Self::check_fmode_for_setlk(file.as_ref(), req.req_type)?;
        let key = Self::key_from_file(file.as_ref());

        let entry = if matches!(req.req_type, PosixLockRequestType::Unlock) {
            let Some(entry) = self.get_entry(&key) else {
                return Ok(());
            };
            entry
        } else {
            self.get_or_create_entry(key)
        };

        let changed = if matches!(req.req_type, PosixLockRequestType::Unlock) {
            entry
                .state
                .lock()
                .apply_owner_request(owner_id, owner_pid, req)
        } else {
            self.lock_or_wait(&entry, owner_id, owner_pid, req, blocking)?
        };

        if changed {
            entry.waitq.wakeup_all(None);
        }

        self.try_cleanup_entry(&key, &entry);
        Ok(())
    }

    pub fn release_owner_for_file(&self, file: &Arc<File>, owner_id: usize) {
        let key = Self::key_from_file(file.as_ref());
        let Some(entry) = self.get_entry(&key) else {
            return;
        };

        let changed = entry.state.lock().remove_all_for_owner(owner_id);
        if changed {
            entry.waitq.wakeup_all(None);
        }

        self.try_cleanup_entry(&key, &entry);
    }
}

fn parse_flock_to_range(
    file: &File,
    flock: &PosixFlock,
    allow_unlock: bool,
) -> Result<ParsedRangeLock, SystemError> {
    let req_type = PosixLockRequestType::from_user_type(flock.l_type).ok_or(SystemError::EINVAL)?;
    if !allow_unlock && matches!(req_type, PosixLockRequestType::Unlock) {
        return Err(SystemError::EINVAL);
    }

    let base: i64 = match flock.l_whence as u32 {
        SEEK_SET => 0,
        SEEK_CUR => {
            let pos = file.pos();
            if pos > i64::MAX as usize {
                return Err(SystemError::EOVERFLOW);
            }
            pos as i64
        }
        SEEK_END => file.metadata()?.size,
        _ => return Err(SystemError::EINVAL),
    };

    let absolute_start = (base as i128) + (flock.l_start as i128);
    if absolute_start > i64::MAX as i128 {
        return Err(SystemError::EOVERFLOW);
    }
    if absolute_start < 0 {
        return Err(SystemError::EINVAL);
    }
    let mut start = absolute_start as i64;
    let end: i64;

    if flock.l_len > 0 {
        let req_len_minus1 = (flock.l_len as i128) - 1;
        let absolute_end = (start as i128) + req_len_minus1;
        if absolute_end > i64::MAX as i128 {
            return Err(SystemError::EOVERFLOW);
        }
        end = absolute_end as i64;
    } else if flock.l_len < 0 {
        let neg_start = (start as i128) + (flock.l_len as i128);
        if neg_start < 0 {
            return Err(SystemError::EINVAL);
        }
        end = start - 1;
        start = neg_start as i64;
    } else {
        end = i64::MAX;
    }

    Ok(ParsedRangeLock {
        req_type,
        start,
        end,
    })
}

static POSIX_LOCK_MANAGER: Lazy<PosixLockManager> = Lazy::new();

pub fn init_posix_lock_manager() {
    if !POSIX_LOCK_MANAGER.initialized() {
        POSIX_LOCK_MANAGER.init(PosixLockManager::new());
    }
}

#[inline]
pub fn get_posix_lock(
    file: &Arc<File>,
    owner_id: usize,
    flock: &mut PosixFlock,
) -> Result<(), SystemError> {
    POSIX_LOCK_MANAGER.get().get_lock(file, owner_id, flock)
}

#[inline]
pub fn set_posix_lock(
    file: &Arc<File>,
    owner_id: usize,
    owner_pid: i32,
    flock: &PosixFlock,
    blocking: bool,
) -> Result<(), SystemError> {
    POSIX_LOCK_MANAGER
        .get()
        .set_lock(file, owner_id, owner_pid, flock, blocking)
}

pub fn release_posix_for_file_owner(file: &Arc<File>, owner_id: usize) {
    if !POSIX_LOCK_MANAGER.initialized() {
        return;
    }
    POSIX_LOCK_MANAGER
        .get()
        .release_owner_for_file(file, owner_id);
}
