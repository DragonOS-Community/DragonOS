//! Stable allocation ownership for stacked I/O and asynchronous writeback.
//!
//! This changes allocation lookup, never task membership. A scoped NOIO flag
//! restricts recovery while a stacked caller may still hold FS/MM resources.

use alloc::sync::Arc;
use core::marker::PhantomData;

use crate::process::{ProcessControlBlock, ProcessManager};

use super::{object::ObjectAccount, MemoryAccount};

#[derive(Clone, Debug)]
pub(crate) struct MemoryIoSource {
    account: Arc<MemoryAccount>,
    objects: Arc<ObjectAccount>,
}

impl MemoryIoSource {
    /// Capture both allocation owners from one controller generation. Nested
    /// I/O inherits the already captured source, even after task migration.
    pub(crate) fn current() -> Self {
        let task = ProcessManager::current_pcb();
        if let Some(source) = source_for_task(&task) {
            return source;
        }
        let generation = task
            .task_cgroup_node()
            .resource_domain()
            .effective_memory_generation();
        Self {
            account: generation.account,
            objects: generation.object_account,
        }
    }

    /// Writeback uses the first page's existing memory receipt and entry's
    /// object receipt, analogous to Linux loop's first-bio memcg source.
    pub(crate) fn from_owners(account: &Arc<MemoryAccount>, objects: &Arc<ObjectAccount>) -> Self {
        Self {
            account: account.clone(),
            objects: objects.clone(),
        }
    }

    /// Existing pages remain charged to their old generation. Only new pages
    /// route through its nearest online ancestor; ObjectAccount already has
    /// its own serialized offline reparenting protocol.
    pub(super) fn page_account(&self) -> Arc<MemoryAccount> {
        let mut account = self.account.clone();
        while !account.is_online() {
            let Some(parent) = account.parent() else {
                break;
            };
            account = parent.clone();
        }
        account
    }

    pub(super) fn object_account(&self) -> Arc<ObjectAccount> {
        self.objects.clone()
    }

    pub(crate) fn enter(&self) -> MemoryIoScope {
        MemoryIoScope::install(self.clone(), false)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct MemoryIoContext {
    source: MemoryIoSource,
    noio: bool,
}

pub(super) fn source_for_task(task: &ProcessControlBlock) -> Option<MemoryIoSource> {
    task.memory_io_context
        .lock_irqsave()
        .as_ref()
        .map(|context| context.source.clone())
}

pub(crate) fn noio_active() -> bool {
    ProcessManager::current_pcb()
        .memory_io_context
        .lock_irqsave()
        .as_ref()
        .is_some_and(|context| context.noio)
}

/// Scoped task-local state must retire on the entering task, in nesting order.
/// The guard deliberately cannot move to another executor or thread.
#[must_use]
pub(crate) struct MemoryIoScope {
    task: Arc<ProcessControlBlock>,
    previous: Option<MemoryIoContext>,
    _task_local: PhantomData<*mut ()>,
}

impl MemoryIoScope {
    pub(crate) fn enter_noio() -> Self {
        Self::install(MemoryIoSource::current(), true)
    }

    fn install(source: MemoryIoSource, noio: bool) -> Self {
        let task = ProcessManager::current_pcb();
        let previous = {
            let mut slot = task.memory_io_context.lock_irqsave();
            let noio = noio || slot.as_ref().is_some_and(|context| context.noio);
            slot.replace(MemoryIoContext { source, noio })
        };
        Self {
            task,
            previous,
            _task_local: PhantomData,
        }
    }
}

impl Drop for MemoryIoScope {
    fn drop(&mut self) {
        let retired = {
            let mut slot = self.task.memory_io_context.lock_irqsave();
            core::mem::replace(&mut *slot, self.previous.take())
        };
        // Last owner destruction must not occur under the task state lock.
        drop(retired);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cgroup::memory::MemoryKind;

    #[test]
    fn source_reuses_object_owner_and_routes_only_new_pages_after_offline() {
        let root = MemoryAccount::new(90001, None).unwrap();
        let parent = MemoryAccount::new(90002, Some(root.clone())).unwrap();
        let leaf = MemoryAccount::new(90003, Some(parent.clone())).unwrap();
        let objects = ObjectAccount::new(&leaf).unwrap();
        let source = MemoryIoSource::from_owners(&leaf, &objects);
        let old_page = leaf.try_charge(MemoryKind::File, 1).unwrap();
        assert!(Arc::ptr_eq(&source.page_account(), &leaf));
        assert!(Arc::ptr_eq(&source.object_account(), &objects));
        leaf.offline();
        assert!(Arc::ptr_eq(&source.page_account(), &parent));
        assert!(Arc::ptr_eq(old_page.account(), &leaf));
        parent.offline();
        assert!(Arc::ptr_eq(&source.page_account(), &root));
        assert!(Arc::ptr_eq(&source.clone().object_account(), &objects));
    }
}
