//! Resolve the allocation domain without caching task migration in an mm.

use alloc::sync::Arc;

use crate::process::ProcessControlBlock;

use super::object::ObjectAccount;
use super::MemoryAccount;

/// Memory is a domain controller: threaded tasks charge their resource domain.
/// An inactive controller routes new resources to the nearest enabled ancestor;
/// existing resource tokens never move along with the allocating task.
///
/// No hierarchy update lock or temporary collections are required. Callers
/// must not hold an mm owner lock while accessing task/domain state.
pub(crate) fn account_for_task(task: &ProcessControlBlock) -> Arc<MemoryAccount> {
    if let Some(source) = super::io::source_for_task(task) {
        return source.page_account();
    }
    task.task_cgroup_node().resource_domain().memory_account()
}

pub(crate) fn object_for_task(task: &ProcessControlBlock) -> Arc<ObjectAccount> {
    if let Some(source) = super::io::source_for_task(task) {
        return source.object_account();
    }
    task.task_cgroup_node()
        .resource_domain()
        .effective_memory_generation()
        .object_account
}
