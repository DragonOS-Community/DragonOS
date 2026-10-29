//! Task attachment ownership and explicit, per-set exit replay.
use super::SemUndoGroup;
use crate::{
    ipc::sem::{SemManager, SemWakeBatch},
    process::{
        namespace::ipc_namespace::IpcNamespace,
        pid::{Pid, PidType},
        ProcessControlBlock,
    },
};
use alloc::sync::Arc;

#[derive(Debug)]
pub struct SemUndoAttachment {
    group: Arc<SemUndoGroup>,
}

pub(crate) struct UnpublishedSemUndoAttachmentGuard {
    group: Arc<SemUndoGroup>,
    attachment: Option<SemUndoAttachment>,
    installed_child: Option<Arc<ProcessControlBlock>>,
    armed: bool,
}

impl SemUndoAttachment {
    pub(crate) fn new(group: Arc<SemUndoGroup>) -> Self {
        Self { group }
    }

    pub(crate) fn group(&self) -> Arc<SemUndoGroup> {
        self.group.clone()
    }

    #[cfg(test)]
    pub(super) fn new_for_test(group: Arc<SemUndoGroup>) -> Self {
        Self::new(group)
    }

    #[cfg(test)]
    pub(super) fn group_for_test(&self) -> Arc<SemUndoGroup> {
        self.group()
    }
}

impl Drop for SemUndoAttachment {
    // Replay is an explicit lifecycle operation and must never run from Drop.
    fn drop(&mut self) {}
}

pub(crate) fn detach_sem_undo(pcb: &Arc<ProcessControlBlock>) {
    if let Some(replay) = PendingSemUndoReplay::detach(pcb) {
        replay.replay();
    }
}

/// Logical detachment may happen under publication locks; actual replay must not.
/// Pin the old namespace and actor before publishing new task namespace state.
/// Execution is explicit: dropping this context never runs semaphore operations.
#[must_use = "detached final-owner undo debt must be replayed explicitly"]
pub(crate) struct PendingSemUndoReplay {
    group: Arc<SemUndoGroup>,
    ipc_ns: Option<Arc<IpcNamespace>>,
    actor: Option<Arc<Pid>>,
}

impl PendingSemUndoReplay {
    pub(crate) fn detach(pcb: &Arc<ProcessControlBlock>) -> Option<Self> {
        let attachment = pcb.take_sem_undo_attachment()?;
        let group = attachment.group();
        drop(attachment);
        if !group.detach_owner_and_mark_last() {
            return None;
        }
        Some(Self::new(pcb, group))
    }

    fn new(pcb: &Arc<ProcessControlBlock>, group: Arc<SemUndoGroup>) -> Self {
        let ipc_ns = group.ipc_ns.upgrade();
        let actor = pcb.try_active_pid_ns().and_then(|pid_ns| {
            pcb.task_pid_nr_ns(PidType::TGID, Some(pid_ns))
                .filter(|tgid| tgid.data() != 0)?;
            pcb.task_pid_ptr(PidType::TGID)
        });
        Self {
            group,
            ipc_ns,
            actor,
        }
    }

    pub(crate) fn replay(self) {
        let Self {
            group,
            ipc_ns,
            actor,
        } = self;
        if !group.begin_replay() {
            return;
        }
        let Some(ipc_ns) = ipc_ns else {
            drop(group.discard_retired_records());
            return;
        };

        loop {
            let mut wakes = SemWakeBatch::default();
            let record = {
                let mut manager = ipc_ns.sem.lock();
                let Some(record) = group.pop_retired_record() else {
                    break;
                };
                SemManager::replay_sem_undo_adjustments(
                    &mut manager,
                    record.semid,
                    &record.adjustments,
                    actor.clone(),
                    &mut wakes,
                );
                manager.unregister_undo_group(record.semid, &group);
                record
            };
            // Publish and notify one set at a time, like Linux exit_sem. Pending
            // records stay in the group so interleaved semctl still clears them.
            wakes.wake_all();
            SemManager::shrink_undo_registry(&ipc_ns, record.semid);
        }
    }
}

#[cfg(test)]
pub(super) fn replay_marked_records(pcb: &Arc<ProcessControlBlock>, group: &Arc<SemUndoGroup>) {
    PendingSemUndoReplay::new(pcb, group.clone()).replay();
}

impl UnpublishedSemUndoAttachmentGuard {
    pub(crate) fn new(group: Arc<SemUndoGroup>) -> Self {
        group.acquire_shared_owner();

        Self {
            attachment: Some(SemUndoAttachment::new(group.clone())),
            group,
            installed_child: None,
            armed: true,
        }
    }

    pub(crate) fn install_into(&mut self, child: &ProcessControlBlock) {
        assert!(self.armed, "cannot install a disarmed SEM_UNDO guard");
        assert!(
            self.installed_child.is_none(),
            "SEM_UNDO guard can only be installed once"
        );
        let attachment = self
            .attachment
            .take()
            .expect("SEM_UNDO guard attachment token is missing");
        self.installed_child = Some(child.install_unpublished_sem_undo_attachment(attachment));
    }

    pub(crate) fn disarm(mut self) {
        debug_assert!(
            self.attachment.is_none() && self.installed_child.is_some(),
            "only an installed SEM_UNDO guard can be disarmed"
        );
        self.armed = false;
    }
}

impl Drop for UnpublishedSemUndoAttachmentGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }

        if let Some(child) = self.installed_child.take() {
            let attachment = child.take_sem_undo_attachment();
            debug_assert!(attachment.is_some(), "installed SEM_UNDO slot is empty");
            if let Some(attachment) = attachment {
                debug_assert!(Arc::ptr_eq(&attachment.group, &self.group));
                drop(attachment);
            }
        } else {
            drop(self.attachment.take());
        }

        self.group.rollback_unpublished_owner();
    }
}
