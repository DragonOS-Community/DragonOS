//! Cgroup Namespace Implementation
//!
//! This module provides cgroup namespace support for process isolation.
//! Currently implemented as a bypass/stub to allow container runtimes (like runcell)
//! to function, with extensibility for future full cgroup implementation.
//!
//! In Linux, cgroup namespaces virtualize the view of a process's cgroups.
//! When a process creates a new cgroup namespace, its current cgroups directories
//! become the cgroup root directories of the new namespace.
//!
//! Reference: https://man7.org/linux/man-pages/man7/cgroup_namespaces.7.html

use alloc::sync::{Arc, Weak};
use system_error::SystemError;

use crate::process::fork::CloneFlags;
use crate::process::namespace::nsproxy::NsCommon;
use crate::process::namespace::user_namespace::INIT_USER_NAMESPACE;
use crate::process::namespace::{NamespaceOps, NamespaceType};

use super::user_namespace::UserNamespace;

lazy_static! {
    /// Initial cgroup namespace for the root process.
    /// All processes start in this namespace unless they create a new one.
    pub static ref INIT_CGROUP_NAMESPACE: Arc<CgroupNamespace> = CgroupNamespace::new_root();
}

/// Cgroup Namespace structure.
///
/// This provides isolation for the cgroup filesystem view.
/// Currently implemented as a stub for bypass purposes, but designed
/// for extensibility when full cgroup support is added.
///
/// Future extensions should add:
/// - `root_cset`: CSS set representing the root cgroup in this namespace
/// - Integration with actual cgroup controllers
pub struct CgroupNamespace {
    /// Common namespace fields (level, type, nsid)
    ns_common: NsCommon,

    /// Self reference for Arc::new_cyclic pattern
    self_ref: Weak<CgroupNamespace>,

    /// Associated user namespace for permission checks.
    /// Required for CAP_SYS_ADMIN validation when creating/joining namespaces.
    user_ns: Arc<UserNamespace>,
    // Future fields for full cgroup implementation:
    // /// Root CSS set for this namespace
    // root_cset: Option<Arc<CssSet>>,
}

impl NamespaceOps for CgroupNamespace {
    fn ns_common(&self) -> &NsCommon {
        &self.ns_common
    }
}

impl CgroupNamespace {
    /// Create the root (initial) cgroup namespace.
    /// This is used for the init process and serves as the ancestor
    /// of all other cgroup namespaces.
    fn new_root() -> Arc<Self> {
        Arc::new_cyclic(|weak_self| Self {
            ns_common: NsCommon::new(0, NamespaceType::Cgroup),
            self_ref: weak_self.clone(),
            user_ns: INIT_USER_NAMESPACE.clone(),
        })
    }

    /// Copy/create a cgroup namespace based on clone flags.
    ///
    /// If CLONE_NEWCGROUP is set, creates a new cgroup namespace.
    /// Otherwise, returns a reference to the current namespace.
    ///
    /// # Arguments
    /// * `clone_flags` - Clone flags indicating whether to create new namespace
    /// * `user_ns` - User namespace for permission checks
    ///
    /// # Returns
    /// * `Ok(Arc<CgroupNamespace>)` - The (possibly new) cgroup namespace
    /// * `Err(SystemError)` - If namespace creation fails
    ///
    /// Reference: https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/cgroup/namespace.c#50
    pub fn copy_cgroup_ns(
        &self,
        clone_flags: &CloneFlags,
        user_ns: Arc<UserNamespace>,
    ) -> Result<Arc<CgroupNamespace>, SystemError> {
        // If CLONE_NEWCGROUP is not set, share the parent's namespace
        if !clone_flags.contains(CloneFlags::CLONE_NEWCGROUP) {
            return Ok(self.self_ref.upgrade().unwrap());
        }

        // Create a new cgroup namespace
        // In Linux, this would:
        // 1. Check CAP_SYS_ADMIN capability
        // 2. Get current process's css_set as the root
        // 3. Allocate new namespace structure
        //
        // For now, we create a stub namespace that allows container
        // runtimes to function while maintaining the interface for
        // future full implementation.

        // TODO: Add capability check when capability system is more complete
        // if !ns_capable(user_ns, CAP_SYS_ADMIN) {
        //     return Err(SystemError::EPERM);
        // }

        let new_ns = Arc::new_cyclic(|weak_self| CgroupNamespace {
            ns_common: NsCommon::new(self.ns_common.level + 1, NamespaceType::Cgroup),
            self_ref: weak_self.clone(),
            user_ns,
        });

        Ok(new_ns)
    }

    /// Get the owning user namespace.
    /// Used for permission checks in setns operations.
    pub fn user_ns(&self) -> &Arc<UserNamespace> {
        &self.user_ns
    }

    /// Get the namespace level (depth in hierarchy).
    /// Root namespace has level 0.
    pub fn level(&self) -> u32 {
        self.ns_common.level
    }
}

// Implement Send and Sync for CgroupNamespace
// This is safe because:
// - NsCommon contains only primitive types and Arc
// - Weak<CgroupNamespace> is Send + Sync
// - Arc<UserNamespace> is Send + Sync
unsafe impl Send for CgroupNamespace {}
unsafe impl Sync for CgroupNamespace {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_init_cgroup_namespace() {
        let ns = INIT_CGROUP_NAMESPACE.clone();
        assert_eq!(ns.level(), 0);
        assert_eq!(ns.ns_common().ty(), NamespaceType::Cgroup);
    }
}
