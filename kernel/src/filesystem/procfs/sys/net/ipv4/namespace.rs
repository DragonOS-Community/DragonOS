//! Namespace identity shared by an IPv4 sysctl directory and its files.

use alloc::sync::{Arc, Weak};
use system_error::SystemError;

use crate::process::{
    cred::{ns_capable, CAPFlags},
    namespace::{
        net_namespace::NetNamespace,
        user_namespace::{make_kgid, make_kuid},
    },
    ProcessManager,
};

/// Cached directories and inodes do not keep a namespace alive. A directory
/// lookup inherits this identity even through dirfd, cwd or a bind mount.
#[derive(Debug, Clone)]
pub(super) struct SysctlNamespace(Weak<NetNamespace>);

impl SysctlNamespace {
    pub(super) fn current() -> Self {
        Self(Arc::downgrade(&ProcessManager::current_netns()))
    }

    pub(super) fn upgrade(&self) -> Result<Arc<NetNamespace>, SystemError> {
        self.0.upgrade().ok_or(SystemError::ENOENT)
    }

    pub(super) fn is_current(&self) -> bool {
        self.0
            .upgrade()
            .is_some_and(|ns| Arc::ptr_eq(&ns, &ProcessManager::current_netns()))
    }

    pub(super) fn access_bits(&self) -> Result<u32, SystemError> {
        Ok(Self::access_bits_for(&self.upgrade()?))
    }

    pub(super) fn access_bits_for(ns: &Arc<NetNamespace>) -> u32 {
        if ProcessManager::current_pcb().cred().euid.data() == 0
            || ns_capable(ns.user_ns(), CAPFlags::CAP_NET_ADMIN)
        {
            6
        } else {
            4
        }
    }

    pub(super) fn owner(&self) -> Option<(usize, usize)> {
        let ns = self.0.upgrade()?;
        Some((
            make_kuid(ns.user_ns(), 0).map_or(0, |id| id.data()),
            make_kgid(ns.user_ns(), 0).map_or(0, |id| id.data()),
        ))
    }
}
