//! Inode-wide exclusion between executable images and writable open files.
//!
//! The registry owns only counts. Guards retain the canonical inode, so
//! identities cannot be recycled while access is outstanding.
//! No filesystem operation or inode destruction runs under the registry lock.

use alloc::sync::Arc;
use hashbrown::HashMap;
use system_error::SystemError;

use super::{
    inode_lifecycle::{InodeRetentionGuard, InodeRetentionKind},
    mount::MountFSInode,
    IndexNode,
};
use crate::libs::{casting::DowncastArc, mutex::Mutex};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum InodeIdentity {
    // This coordinator is already shared by all aliases, including overlay
    // copy-up aliases. Only its identity is used; its mutation lock is not.
    Coordinator(usize),
    // Inodes without a coordinator are single-edge pseudo/anonymous objects.
    // Their allocation is canonical, and some deliberately have no fs().
    SingleInode(usize),
}

// Positive counts are writers; negative counts deny writers for exec.
static ACCESS: Mutex<Option<HashMap<InodeIdentity, isize>>> = Mutex::new(None);

#[derive(Debug)]
pub struct InodeWriteGuard {
    key: InodeIdentity,
    writer: bool,
    _inode: InodeRetentionGuard,
}

impl InodeWriteGuard {
    pub fn writer(inode: Arc<dyn IndexNode>) -> Result<Arc<Self>, SystemError> {
        Self::acquire(inode, true)
    }

    pub fn deny_write(inode: Arc<dyn IndexNode>) -> Result<Arc<Self>, SystemError> {
        Self::acquire(inode, false)
    }

    fn acquire(mut inode: Arc<dyn IndexNode>, writer: bool) -> Result<Arc<Self>, SystemError> {
        while let Some(mounted) = inode.clone().downcast_arc::<MountFSInode>() {
            inode = mounted.underlying_inode();
        }
        let retention = InodeRetentionGuard::new(inode.clone(), InodeRetentionKind::Operation)?;
        let key = match inode.link_mutation_coordinator() {
            Some(coordinator) => InodeIdentity::Coordinator(coordinator as *const _ as usize),
            None => InodeIdentity::SingleInode(Arc::as_ptr(&inode) as *const () as usize),
        };
        {
            let mut registry = ACCESS.lock();
            let registry = registry.get_or_insert_with(HashMap::new);
            let count = registry.get(&key).copied().unwrap_or(0);
            if (writer && count < 0) || (!writer && count > 0) {
                return Err(SystemError::ETXTBSY);
            }
            let count = count
                .checked_add(if writer { 1 } else { -1 })
                .ok_or(SystemError::EOVERFLOW)?;
            if !registry.contains_key(&key) {
                registry.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
            }
            registry.insert(key, count);
        }
        // On allocation failure the value is dropped and rolls the count back.
        Arc::try_new(Self {
            key,
            writer,
            _inode: retention,
        })
        .map_err(|_| SystemError::ENOMEM)
    }
}

impl Drop for InodeWriteGuard {
    fn drop(&mut self) {
        let mut registry = ACCESS.lock();
        let registry = registry.as_mut().expect("write-access registry exists");
        let count = registry
            .get_mut(&self.key)
            .expect("write-access owner exists");
        *count += if self.writer { -1 } else { 1 };
        if *count == 0 {
            registry.remove(&self.key);
        }
    }
}
