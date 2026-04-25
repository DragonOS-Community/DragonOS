use crate::libs::rwsem::RwSem;
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::{collections::btree_map::Entry, format};
use system_error::SystemError;

/// Per-network-namespace abstract UNIX address table.
///
/// Linux scopes AF_UNIX abstract namespace addresses to the network namespace.
#[derive(Debug)]
pub struct UnixAbstractTable {
    handles: RwSem<BTreeMap<Arc<[u8]>, Weak<AbstractHandle>>>,
    nsid: usize,
}

/// Unix Socket的抽象路径
#[derive(Debug)]
pub struct AbstractHandle {
    name: Arc<[u8]>,
    table: Weak<UnixAbstractTable>,
    nsid: usize,
}

impl AbstractHandle {
    fn new(name: Arc<[u8]>, table: Weak<UnixAbstractTable>, nsid: usize) -> Self {
        Self { name, table, nsid }
    }

    pub fn name(&self) -> Arc<[u8]> {
        self.name.clone()
    }

    pub fn nsid(&self) -> usize {
        self.nsid
    }
}

impl Drop for AbstractHandle {
    fn drop(&mut self) {
        if let Some(table) = self.table.upgrade() {
            table.remove_if_unused(&self.name);
        }
    }
}

impl PartialEq for AbstractHandle {
    fn eq(&self, other: &Self) -> bool {
        self.nsid == other.nsid && self.name.as_ref() == other.name.as_ref()
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
        (self.nsid, self.name.as_ref()).cmp(&(other.nsid, other.name.as_ref()))
    }
}

impl UnixAbstractTable {
    pub fn new(nsid: usize) -> Arc<Self> {
        Arc::new(Self {
            handles: RwSem::new(BTreeMap::new()),
            nsid,
        })
    }

    fn create(self: &Arc<Self>, name: Arc<[u8]>) -> Option<Arc<AbstractHandle>> {
        let mut handles = self.handles.write();
        let mut entry = handles.entry(name.clone());

        if let Entry::Occupied(ref occupied) = entry {
            // 如果引用计数大于0，说明名字已经被占用
            if occupied.get().strong_count() > 0 {
                return None;
            }
        }

        let new_handle = Arc::new(AbstractHandle::new(name, Arc::downgrade(self), self.nsid));
        let weak_handle = Arc::downgrade(&new_handle);

        match entry {
            Entry::Occupied(ref mut occupied) => {
                occupied.insert(weak_handle);
            }
            Entry::Vacant(vacant) => {
                vacant.insert(weak_handle);
            }
        }

        Some(new_handle)
    }

    fn lookup(&self, name: &[u8]) -> Option<Arc<AbstractHandle>> {
        let handles = self.handles.read();
        handles.get(name).and_then(Weak::upgrade)
    }

    fn remove_if_unused(&self, name: &Arc<[u8]>) {
        let mut handles = self.handles.write();

        let Some(weak) = handles.get(name) else {
            return;
        };

        // 如果引用计数为0，说明名字已经不再使用，可以移除
        if weak.strong_count() == 0 {
            handles.remove(name);
        }
    }

    pub fn create_abstract_name_bytes(
        self: &Arc<Self>,
        name: &[u8],
    ) -> Result<Arc<AbstractHandle>, SystemError> {
        let name = Arc::from(name);
        self.create(name).ok_or(SystemError::EADDRINUSE)
    }

    pub fn alloc_ephemeral_abstract_name(
        self: &Arc<Self>,
    ) -> Result<Arc<AbstractHandle>, SystemError> {
        // todo 随机化
        // 尝试分配一个临时的抽象名字
        (0..(1 << 20))
            .map(|num| format!("{:05x}", num))
            .map(|name| Arc::from(name.as_bytes()))
            .filter_map(|name| self.create(name))
            .next()
            .ok_or(SystemError::ECONNREFUSED)
    }

    pub fn lookup_abstract_name_bytes(
        &self,
        name: &[u8],
    ) -> Result<Arc<AbstractHandle>, SystemError> {
        self.lookup(name).ok_or(SystemError::ECONNREFUSED)
    }
}
