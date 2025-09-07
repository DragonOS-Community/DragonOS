use crate::libs::rwlock::RwLock;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::{collections::btree_map::Entry, format};
use system_error::SystemError;

/// Unix Socket的抽象路径
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct AbstractHandle {
    name: Arc<[u8]>,
}

impl AbstractHandle {
    fn new(name: Arc<[u8]>) -> Self {
        Self { name }
    }

    pub fn name(&self) -> Arc<[u8]> {
        self.name.clone()
    }
}

impl Drop for AbstractHandle {
    fn drop(&mut self) {
        HANDLE_TABLE.remove(self.name());
    }
}

static HANDLE_TABLE: HandleTable = HandleTable::new();

struct HandleTable {
    handles: RwLock<BTreeMap<Arc<[u8]>, Weak<AbstractHandle>>>,
}

impl HandleTable {
    const fn new() -> Self {
        Self {
            handles: RwLock::new(BTreeMap::new()),
        }
    }

    fn create(&self, name: Arc<[u8]>) -> Option<Arc<AbstractHandle>> {
        let mut handles = self.handles.write();

        let mut entry = handles.entry(name.clone());

        if let Entry::Occupied(ref occupied) = entry {
            // 如果引用计数大于0，说明名字已经被占用
            if occupied.get().strong_count() > 0 {
                return None;
            }
        }

        let new_handle = Arc::new(AbstractHandle::new(name));
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

    fn remove(&self, name: Arc<[u8]>) {
        let mut handles = self.handles.write();

        let Entry::Occupied(occupied) = handles.entry(name) else {
            return;
        };

        // 如果引用计数为0，说明名字已经不再使用，可以移除
        if occupied.get().strong_count() == 0 {
            occupied.remove();
        }
    }

    fn lookup(&self, name: &[u8]) -> Option<Arc<AbstractHandle>> {
        let handles = self.handles.read();

        handles.get(name).and_then(Weak::upgrade)
    }

    fn alloc_ephemeral(&self) -> Option<Arc<AbstractHandle>> {
        // todo 随机化
        // 尝试分配一个临时的抽象名字
        (0..(1 << 20))
            .map(|num| format!("{:05x}", num))
            .map(|name| Arc::from(name.as_bytes()))
            .filter_map(|name| self.create(name))
            .next()
    }
}

pub fn create_abstract_name(name: String) -> Result<Arc<AbstractHandle>, SystemError> {
    let name = Arc::from(name.into_bytes().as_slice());
    HANDLE_TABLE.create(name).ok_or(SystemError::EADDRINUSE)
}

pub fn alloc_ephemeral_abstract_name() -> Result<Arc<AbstractHandle>, SystemError> {
    HANDLE_TABLE
        .alloc_ephemeral()
        .ok_or(SystemError::ECONNREFUSED)
}

pub fn lookup_abstract_name(name: &[u8]) -> Result<Arc<AbstractHandle>, SystemError> {
    HANDLE_TABLE.lookup(name).ok_or(SystemError::ECONNREFUSED)
}
