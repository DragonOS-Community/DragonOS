//! Mount ownership conversion, matching Linux 6.6 `fs/mnt_idmapping.c`.
//!
//! Backing inodes retain filesystem kernel-global IDs. Mount views also use
//! kernel-global IDs, so caller namespace conversion belongs at the syscall
//! boundary. Missing mappings remain `None`, never an overflow UID or GID.

use alloc::sync::Arc;

use crate::process::namespace::user_namespace::{
    map_id_down, map_id_up, UserNamespace, INIT_USER_NAMESPACE,
};

/// An immutable mapping owner; its UID/GID maps may still be written once.
///
/// Retain the namespace rather than copying its maps: Linux permits installing
/// an idmap before the namespace's first map write. This object neither holds
/// backend references nor accesses the current process, including at boot.
pub struct MountIdmap {
    owner: Arc<UserNamespace>,
}

/// Validate an ID on a non-idmapped mount, including identity fast paths.
pub fn identity_id(id: usize) -> Option<usize> {
    u32::try_from(id)
        .ok()
        .filter(|id| *id != u32::MAX)
        .map(|id| id as usize)
}

impl MountIdmap {
    pub fn new(owner: Arc<UserNamespace>) -> Self {
        Self { owner }
    }

    pub fn owner(&self) -> &Arc<UserNamespace> {
        &self.owner
    }

    fn is_identity(&self, fs_userns: &Arc<UserNamespace>) -> bool {
        Arc::ptr_eq(&self.owner, &INIT_USER_NAMESPACE) || Arc::ptr_eq(&self.owner, fs_userns)
    }

    /// Filesystem kernel-global UID to mount-view kernel-global UID.
    pub fn uid_into_view(&self, fs_userns: &Arc<UserNamespace>, uid: usize) -> Option<usize> {
        let uid = identity_id(uid)? as u32;
        if self.is_identity(fs_userns) {
            return Some(uid as usize);
        }
        let local = if Arc::ptr_eq(fs_userns, &INIT_USER_NAMESPACE) {
            uid
        } else {
            // Release the filesystem namespace lock before taking owner's.
            let inner = fs_userns.inner.lock();
            map_id_up(&inner.uid_map, uid)?
        };
        let global = {
            let inner = self.owner.inner.lock();
            map_id_down(&inner.uid_map, local)?
        };
        identity_id(global as usize)
    }

    /// Mount-view kernel-global UID to filesystem kernel-global UID.
    pub fn uid_from_view(&self, fs_userns: &Arc<UserNamespace>, uid: usize) -> Option<usize> {
        let uid = identity_id(uid)? as u32;
        if self.is_identity(fs_userns) {
            return Some(uid as usize);
        }
        let local = {
            let inner = self.owner.inner.lock();
            map_id_up(&inner.uid_map, uid)?
        };
        if Arc::ptr_eq(fs_userns, &INIT_USER_NAMESPACE) {
            return identity_id(local as usize);
        }
        let global = {
            let inner = fs_userns.inner.lock();
            map_id_down(&inner.uid_map, local)?
        };
        identity_id(global as usize)
    }

    /// Filesystem kernel-global GID to mount-view kernel-global GID.
    pub fn gid_into_view(&self, fs_userns: &Arc<UserNamespace>, gid: usize) -> Option<usize> {
        let gid = identity_id(gid)? as u32;
        if self.is_identity(fs_userns) {
            return Some(gid as usize);
        }
        let local = if Arc::ptr_eq(fs_userns, &INIT_USER_NAMESPACE) {
            gid
        } else {
            let inner = fs_userns.inner.lock();
            map_id_up(&inner.gid_map, gid)?
        };
        let global = {
            let inner = self.owner.inner.lock();
            map_id_down(&inner.gid_map, local)?
        };
        identity_id(global as usize)
    }

    /// Mount-view kernel-global GID to filesystem kernel-global GID.
    pub fn gid_from_view(&self, fs_userns: &Arc<UserNamespace>, gid: usize) -> Option<usize> {
        let gid = identity_id(gid)? as u32;
        if self.is_identity(fs_userns) {
            return Some(gid as usize);
        }
        let local = {
            let inner = self.owner.inner.lock();
            map_id_up(&inner.gid_map, gid)?
        };
        if Arc::ptr_eq(fs_userns, &INIT_USER_NAMESPACE) {
            return identity_id(local as usize);
        }
        let global = {
            let inner = fs_userns.inner.lock();
            map_id_down(&inner.gid_map, local)?
        };
        identity_id(global as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::{
        cred::INIT_CRED,
        namespace::user_namespace::{UidGidExtent, UidGidMap},
    };
    use core::sync::atomic::Ordering;

    fn map(extents: &[(u32, u32, u32)]) -> UidGidMap {
        let mut result = UidGidMap::default();
        assert!(extents.len() <= result.extent.len());
        for (slot, &(first, lower_first, count)) in result.extent.iter_mut().zip(extents) {
            *slot = UidGidExtent {
                first,
                lower_first,
                count,
            };
        }
        result
            .nr_extents
            .store(extents.len() as u32, Ordering::Release);
        result
    }

    fn namespace(uid: &[(u32, u32, u32)], gid: &[(u32, u32, u32)]) -> Arc<UserNamespace> {
        let ns = UserNamespace::create_user_ns(&INIT_CRED).unwrap();
        {
            let mut inner = ns.inner.lock();
            inner.uid_map = map(uid);
            inner.gid_map = map(gid);
        }
        ns
    }

    #[test]
    fn initial_filesystem_uses_distinct_uid_and_gid_maps() {
        let idmap = MountIdmap::new(namespace(&[(0, 1000, 10)], &[(0, 2000, 10)]));
        let fs = &INIT_USER_NAMESPACE;
        assert_eq!(idmap.uid_into_view(fs, 3), Some(1003));
        assert_eq!(idmap.gid_into_view(fs, 3), Some(2003));
        assert_eq!(idmap.uid_from_view(fs, 1003), Some(3));
        assert_eq!(idmap.gid_from_view(fs, 2003), Some(3));
        assert_eq!(idmap.uid_into_view(fs, 10), None);
        assert_eq!(idmap.uid_from_view(fs, 2003), None);
    }

    #[test]
    fn noninitial_filesystem_roundtrip_uses_local_ids_and_multiple_extents() {
        let fs = namespace(&[(0, 5000, 10), (100, 9000, 10)], &[(0, 6000, 10)]);
        let idmap = MountIdmap::new(namespace(
            &[(0, 1000, 10), (100, 3000, 10)],
            &[(0, 2000, 10)],
        ));
        assert_eq!(idmap.uid_into_view(&fs, 5003), Some(1003));
        assert_eq!(idmap.uid_from_view(&fs, 1003), Some(5003));
        assert_eq!(idmap.uid_into_view(&fs, 9007), Some(3007));
        assert_eq!(idmap.uid_from_view(&fs, 3007), Some(9007));
        assert_eq!(idmap.gid_into_view(&fs, 6003), Some(2003));
        assert_eq!(idmap.gid_from_view(&fs, 2003), Some(6003));
        assert_eq!(idmap.uid_into_view(&fs, 5010), None);
        assert_eq!(idmap.uid_from_view(&fs, 3010), None);
    }

    #[test]
    fn mapping_can_be_written_after_owner_is_retained() {
        let owner = namespace(&[], &[]);
        let idmap = MountIdmap::new(owner.clone());
        assert_eq!(idmap.uid_into_view(&INIT_USER_NAMESPACE, 2), None);
        assert_eq!(idmap.gid_from_view(&INIT_USER_NAMESPACE, 2002), None);
        {
            let mut inner = owner.inner.lock();
            inner.uid_map = map(&[(0, 1000, 10)]);
            inner.gid_map = map(&[(0, 2000, 10)]);
        }
        drop(owner);
        assert_eq!(idmap.uid_into_view(&INIT_USER_NAMESPACE, 2), Some(1002));
        assert_eq!(idmap.gid_from_view(&INIT_USER_NAMESPACE, 2002), Some(2));
    }

    #[test]
    fn identity_paths_preserve_ids_but_reject_invalid_values() {
        let fs = namespace(&[(0, 5000, 10)], &[(0, 6000, 10)]);
        // Linux nop mapping bypasses even a noninitial filesystem namespace.
        let initial = MountIdmap::new(INIT_USER_NAMESPACE.clone());
        let same_owner = MountIdmap::new(fs.clone());
        for idmap in [&initial, &same_owner] {
            assert_eq!(idmap.uid_into_view(&fs, 42), Some(42));
            assert_eq!(idmap.uid_from_view(&fs, 42), Some(42));
            assert_eq!(idmap.gid_into_view(&fs, 42), Some(42));
            assert_eq!(idmap.gid_from_view(&fs, 42), Some(42));
            for invalid in [u32::MAX as usize, usize::MAX] {
                assert_eq!(idmap.uid_into_view(&fs, invalid), None);
                assert_eq!(idmap.uid_from_view(&fs, invalid), None);
                assert_eq!(idmap.gid_into_view(&fs, invalid), None);
                assert_eq!(idmap.gid_from_view(&fs, invalid), None);
            }
        }
        assert_eq!(identity_id(42), Some(42));
        assert_eq!(identity_id(u32::MAX as usize), None);
        assert_eq!(identity_id(usize::MAX), None);
        assert_eq!(
            identity_id(u32::MAX as usize - 1),
            Some(u32::MAX as usize - 1)
        );
    }
}
