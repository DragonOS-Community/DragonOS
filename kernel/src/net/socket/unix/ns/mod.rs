use crate::libs::rwsem::RwSem;
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::{collections::btree_map::Entry, format};
use core::sync::atomic::{AtomicU64, Ordering};
use system_error::SystemError;

use super::UnixSocketType;

type AbstractNames = BTreeMap<(UnixSocketType, Arc<[u8]>), Weak<AbstractHandle>>;

/// Per-network-namespace abstract UNIX address table.
///
/// Linux scopes AF_UNIX abstract namespace addresses to the network namespace.
#[derive(Debug)]
pub struct UnixAbstractTable {
    handles: RwSem<AbstractNames>,
    nsid: usize,
    next_incarnation: AtomicU64,
}

/// Unix Socket的抽象路径
#[derive(Debug)]
pub struct AbstractHandle {
    name: Arc<[u8]>,
    table: Weak<UnixAbstractTable>,
    nsid: usize,
    socket_type: UnixSocketType,
    incarnation: u64,
}

impl AbstractHandle {
    fn new(
        name: Arc<[u8]>,
        table: Weak<UnixAbstractTable>,
        nsid: usize,
        socket_type: UnixSocketType,
        incarnation: u64,
    ) -> Self {
        Self {
            name,
            table,
            nsid,
            socket_type,
            incarnation,
        }
    }

    pub fn name(&self) -> Arc<[u8]> {
        self.name.clone()
    }

    pub fn nsid(&self) -> usize {
        self.nsid
    }

    pub(super) fn socket_type(&self) -> UnixSocketType {
        self.socket_type
    }

    pub(super) fn incarnation(&self) -> u64 {
        self.incarnation
    }
}

impl Drop for AbstractHandle {
    fn drop(&mut self) {
        if let Some(table) = self.table.upgrade() {
            table.remove_if_unused(self.socket_type, &self.name);
        }
    }
}

impl PartialEq for AbstractHandle {
    fn eq(&self, other: &Self) -> bool {
        (self.nsid, self.socket_type, self.incarnation)
            == (other.nsid, other.socket_type, other.incarnation)
    }
}

impl Eq for AbstractHandle {}

impl PartialOrd for AbstractHandle {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for AbstractHandle {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        (self.nsid, self.socket_type, self.incarnation).cmp(&(
            other.nsid,
            other.socket_type,
            other.incarnation,
        ))
    }
}

impl UnixAbstractTable {
    pub fn new(nsid: usize) -> Arc<Self> {
        Arc::new(Self {
            handles: RwSem::new(BTreeMap::new()),
            nsid,
            next_incarnation: AtomicU64::new(1),
        })
    }

    fn create(
        self: &Arc<Self>,
        socket_type: UnixSocketType,
        name: Arc<[u8]>,
    ) -> Result<Arc<AbstractHandle>, SystemError> {
        let mut handles = self.handles.write();
        let mut entry = handles.entry((socket_type, name.clone()));

        if let Entry::Occupied(ref occupied) = entry {
            // 如果引用计数大于0，说明名字已经被占用
            if occupied.get().strong_count() > 0 {
                return Err(SystemError::EADDRINUSE);
            }
        }

        let incarnation = self
            .next_incarnation
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                next.checked_add(1)
            })
            .map_err(|_| SystemError::ENOSPC)?;
        let new_handle = Arc::new(AbstractHandle::new(
            name,
            Arc::downgrade(self),
            self.nsid,
            socket_type,
            incarnation,
        ));
        let weak_handle = Arc::downgrade(&new_handle);

        match entry {
            Entry::Occupied(ref mut occupied) => {
                occupied.insert(weak_handle);
            }
            Entry::Vacant(vacant) => {
                vacant.insert(weak_handle);
            }
        }

        Ok(new_handle)
    }

    fn lookup(&self, socket_type: UnixSocketType, name: &[u8]) -> Option<Arc<AbstractHandle>> {
        let handles = self.handles.read();
        handles
            .get(&(socket_type, Arc::from(name)))
            .and_then(Weak::upgrade)
    }

    fn remove_if_unused(&self, socket_type: UnixSocketType, name: &Arc<[u8]>) {
        let mut handles = self.handles.write();

        let key = (socket_type, name.clone());
        let Some(weak) = handles.get(&key) else {
            return;
        };

        // 如果引用计数为0，说明名字已经不再使用，可以移除
        if weak.strong_count() == 0 {
            handles.remove(&key);
        }
    }

    pub(crate) fn create_abstract_name_bytes(
        self: &Arc<Self>,
        socket_type: UnixSocketType,
        name: &[u8],
    ) -> Result<Arc<AbstractHandle>, SystemError> {
        let name = Arc::from(name);
        self.create(socket_type, name)
    }

    pub(crate) fn alloc_ephemeral_abstract_name(
        self: &Arc<Self>,
        socket_type: UnixSocketType,
    ) -> Result<Arc<AbstractHandle>, SystemError> {
        // todo 随机化
        // 尝试分配一个临时的抽象名字
        for num in 0..(1 << 20) {
            let name = format!("{:05x}", num);
            match self.create(socket_type, Arc::from(name.as_bytes())) {
                Ok(handle) => return Ok(handle),
                Err(SystemError::EADDRINUSE) => continue,
                Err(error) => return Err(error),
            }
        }
        Err(SystemError::ENOSPC)
    }

    pub(crate) fn lookup_abstract_name_bytes(
        &self,
        socket_type: UnixSocketType,
        name: &[u8],
    ) -> Result<Arc<AbstractHandle>, SystemError> {
        self.lookup(socket_type, name)
            .ok_or(SystemError::ECONNREFUSED)
    }
}
