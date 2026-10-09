//! cgroup v2 resource-domain topology. Membership still belongs to a task's
//! direct node; only non-threaded controller resources are shared by a domain.
use alloc::sync::Arc;
use core::sync::atomic::Ordering;
use system_error::SystemError;

use super::{cgroup_root, CgroupNode};

pub(crate) fn is_threaded_controller(name: &str) -> bool {
    matches!(name, "cpu" | "cpuset" | "pids")
}

impl CgroupNode {
    pub(crate) fn is_threaded(&self) -> bool {
        self.threaded.load(Ordering::Acquire)
    }

    /// The parent chain is immutable, and topology mutations are serialized by
    /// cgroup::lock(). Do not cache a second domain identity in every task.
    pub(crate) fn resource_domain(self: &Arc<Self>) -> Arc<Self> {
        if !self.is_threaded() {
            return self.clone();
        }
        let mut domain = self.parent().expect("hierarchy root cannot be threaded");
        let mut ancestor = Some(domain.clone());
        while let Some(node) = ancestor {
            // Converting an empty ancestor reassigns ALL threaded descendants,
            // including those separated by now-invalid ordinary domains.
            if node.is_threaded() {
                domain = node.parent().expect("hierarchy root cannot be threaded");
            }
            ancestor = node.parent();
        }
        domain
    }

    pub(crate) fn is_thread_root(&self) -> bool {
        !self.is_threaded()
            && (self.children().iter().any(|child| child.is_threaded())
                || (self.has_tasks()
                    && self
                        .subtree_control()
                        .iter()
                        .any(|name| is_threaded_controller(name))))
    }

    pub(crate) fn is_valid_domain(&self) -> bool {
        if self.is_threaded() {
            return false;
        }
        let mut parent = self.parent();
        while let Some(node) = parent {
            if node.is_threaded() || (node.parent().is_some() && node.is_thread_root()) {
                return false;
            }
            parent = node.parent();
        }
        true
    }

    pub(crate) fn has_populated_domain_children(&self) -> bool {
        self.children()
            .iter()
            .any(|child| !child.is_threaded() && child.subtree_task_count() != 0)
    }

    pub(crate) fn can_be_thread_root(&self) -> bool {
        self.parent().is_none()
            || (!self.is_threaded()
                && !self.has_populated_domain_children()
                && !self
                    .subtree_control()
                    .iter()
                    .any(|name| !is_threaded_controller(name)))
    }

    pub(crate) fn type_name(&self) -> &'static str {
        if self.is_threaded() {
            "threaded"
        } else if !self.is_valid_domain() {
            "domain invalid"
        } else if self.is_thread_root() {
            "domain threaded"
        } else {
            "domain"
        }
    }
}

/// Caller holds cgroup::lock(); no live task may be affected by conversion.
pub(crate) fn enable_threaded(node: &Arc<CgroupNode>) -> Result<(), SystemError> {
    if !cgroup_root().is_online(node) {
        return Err(SystemError::ENOENT);
    }
    let parent = node.parent().ok_or(SystemError::EINVAL)?;
    if node.is_threaded() {
        return Ok(());
    }
    let domain = parent.resource_domain();
    if node.subtree_task_count() != 0
        || node
            .subtree_control()
            .iter()
            .any(|name| !is_threaded_controller(name))
        || !domain.is_valid_domain()
        || !domain.can_be_thread_root()
    {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    // All existing threaded descendants follow the highest threaded ancestor;
    // intervening ordinary descendants become invalid, not resource domains.
    node.threaded.store(true, Ordering::Release);
    Ok(())
}

/// Single-thread migration (including clone into a cgroup) cannot leave its
/// process's shared resource domain. Caller serializes topology and membership.
pub(crate) fn validate_thread_domain(
    src: &Arc<CgroupNode>,
    dst: &Arc<CgroupNode>,
) -> Result<(), SystemError> {
    // A pinned directory fd may outlive both the removed node and its parent.
    // Never follow a removed node's weak parent chain.
    if !cgroup_root().is_online(src) || !cgroup_root().is_online(dst) {
        return Err(SystemError::ENOENT);
    }
    if !Arc::ptr_eq(&src.resource_domain(), &dst.resource_domain()) {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    Ok(())
}
