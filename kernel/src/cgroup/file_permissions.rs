//! Canonical cgroup permission instances prepared without allocator calls
//! under the permission map lock. Controller commit only replaces an entry.
use alloc::{string::String, sync::Arc};
use hashbrown::HashMap;
use system_error::SystemError;

use super::core::CgroupFilePermissions;
use crate::libs::rwlock::RwLock;

pub(super) fn canonical_instance(
    permissions: &RwLock<HashMap<String, Arc<RwLock<CgroupFilePermissions>>>>,
    name: &str,
    default_mode: u32,
) -> Result<Arc<RwLock<CgroupFilePermissions>>, SystemError> {
    if let Some(instance) = permissions.read().get(name).cloned() {
        return Ok(instance);
    }
    let mut key = String::new();
    key.try_reserve_exact(name.len())
        .map_err(|_| SystemError::ENOMEM)?;
    key.push_str(name);
    let attributes = Arc::try_new(RwLock::new(CgroupFilePermissions {
        uid: 0,
        gid: 0,
        mode: default_mode,
        generation: 0,
    }))
    .map_err(|_| SystemError::ENOMEM)?;
    let mut key = Some(key);
    let mut attributes = Some(attributes);
    let mut proposal = HashMap::new();
    let mut allocation_failed = false;
    loop {
        let (instance, needed_capacity) = {
            let mut map = permissions.write();
            if let Some(instance) = map.get(name).cloned() {
                (Some(instance), 0)
            } else {
                let needed = map.len().checked_add(1).ok_or(SystemError::ENOMEM)?;
                if needed > map.capacity() && needed <= proposal.capacity() {
                    // No allocation or element destruction: transfer all
                    // existing ownership into the independently prepared map.
                    for (key, value) in map.drain() {
                        proposal.insert(key, value);
                    }
                    core::mem::swap(&mut *map, &mut proposal);
                }
                if needed <= map.capacity() {
                    let instance = attributes.take().expect("unpublished permission instance");
                    map.insert(
                        key.take().expect("unpublished canonical key"),
                        instance.clone(),
                    );
                    (Some(instance), 0)
                } else {
                    let capacity = needed.max(map.len().checked_mul(2).ok_or(SystemError::ENOMEM)?);
                    (None, capacity)
                }
            }
        };
        // The empty retired backing, unused key and placeholder must never
        // reach their final Drop while holding the permission map lock.
        if let Some(instance) = instance {
            return Ok(instance);
        }
        if allocation_failed {
            return Err(SystemError::ENOMEM);
        }
        // Lazy inode lookup may grow the map concurrently without UPDATE.
        // Recheck its current capacity/key after allocation, and reprepare if
        // necessary rather than assuming that a snapshot reserves its slots.
        proposal = HashMap::new();
        allocation_failed = proposal.try_reserve(needed_capacity).is_err();
    }
}
