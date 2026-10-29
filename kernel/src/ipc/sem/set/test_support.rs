use super::*;
use crate::ipc::sem::manager::SemManager;
use crate::{libs::wait_queue::Waiter, process::namespace::ipc_namespace::IpcNamespace};

use crate::process::{
    cred::{Kgid, Kuid},
    fork::CloneFlags,
    namespace::ipc_namespace::INIT_IPC_NAMESPACE,
    KernelStack, ProcessControlBlock,
};

pub(in crate::ipc::sem) fn test_perm(id: SemId, key: SemKey, seq: usize) -> IpcPerm {
    IpcPerm {
        id: id.data(),
        key: key.data(),
        uid: Kuid::new(0),
        gid: Kgid::new(0),
        cuid: Kuid::new(0),
        cgid: Kgid::new(0),
        mode: 0o600,
        seq,
    }
}

pub(in crate::ipc::sem) fn sem_values(manager: &SemManager, id: SemId) -> Vec<i32> {
    manager
        .get_by_semid_checked(id)
        .unwrap()
        .sems
        .iter()
        .map(|sem| sem.val)
        .collect()
}

pub(in crate::ipc::sem) fn test_ipc_ns() -> Arc<IpcNamespace> {
    INIT_IPC_NAMESPACE.copy_ipc_ns(
        &CloneFlags::CLONE_NEWIPC,
        INIT_IPC_NAMESPACE.user_ns.clone(),
    )
}

pub(in crate::ipc::sem) fn test_pcb_with_group(
    ipc_ns: &Arc<IpcNamespace>,
) -> (Arc<ProcessControlBlock>, Arc<SemUndoGroup>) {
    let pcb = ProcessControlBlock::new_idle(0, KernelStack::new().unwrap());
    let group = pcb.ensure_sem_undo_group(ipc_ns).unwrap();
    (pcb, group)
}

pub(in crate::ipc::sem) fn enqueue_test_waiter(
    set: &mut KernelSemSet,
    sops: &[PosixSemBuf],
    blocker: SemBlockedOp,
) -> Arc<SemQueueEntry> {
    let (_waiter, waker) = Waiter::new_pair();
    let entry = Arc::new(SemQueueEntry::new(sops, None, waker, blocker));
    set.enqueue_waiter(entry.clone());
    entry
}

pub(in crate::ipc::sem) fn enqueue_undo_waiter_for_test(
    manager: &mut SemManager,
    semid: SemId,
    group: &Arc<SemUndoGroup>,
) -> Arc<SemQueueEntry> {
    let (_waiter, waker) = Waiter::new_pair();
    let entry = Arc::new(SemQueueEntry::new_prepared(
        SemQueueEntry::prepare_sops(&[undo_sop(0, -1)]).unwrap(),
        None,
        Some(Arc::clone(group)),
        Some(group.prepare_record_for_test(semid, 1).unwrap()),
        waker,
        SemopScratch::try_new(&[plain_sop(0, 0); 1]).unwrap(),
        SemBlockedOp {
            semnum: 0,
            wait_type: SemWaitType::Increase,
            nowait: false,
        },
    ));
    manager
        .get_by_semid_checked_mut(semid)
        .unwrap()
        .enqueue_waiter(entry.clone());
    entry
}

pub(in crate::ipc::sem) fn undo_sop(sem_num: u16, sem_op: i16) -> PosixSemBuf {
    PosixSemBuf {
        sem_num,
        sem_op,
        sem_flg: SemFlags::SEM_UNDO.bits() as i16,
    }
}

pub(in crate::ipc::sem) fn plain_sop(sem_num: u16, sem_op: i16) -> PosixSemBuf {
    PosixSemBuf {
        sem_num,
        sem_op,
        sem_flg: 0,
    }
}

pub(in crate::ipc::sem) fn nowait_sop(sem_num: u16, sem_op: i16) -> PosixSemBuf {
    PosixSemBuf {
        sem_num,
        sem_op,
        sem_flg: SemFlags::IPC_NOWAIT.bits() as i16,
    }
}

pub(in crate::ipc::sem) fn insert_test_set(
    manager: &mut SemManager,
    key: SemKey,
    vals: &[i32],
) -> SemId {
    manager.insert_test_set(key, vals)
}
pub(in crate::ipc::sem) fn remove_test_set(manager: &mut SemManager, id: SemId) {
    manager.remove_test_set(id);
}
impl KernelSemSet {
    pub(in crate::ipc::sem) fn new_for_test(perm: IpcPerm, vals: &[i32]) -> Self {
        let sems = Self::try_allocate_sems(vals.len()).unwrap();
        let mut set = Self::new(perm, sems);
        for (sem, val) in set.sems.iter_mut().zip(vals.iter().copied()) {
            sem.val = val;
        }
        set
    }
    pub(in crate::ipc::sem) fn undo_groups_for_test(
        &self,
    ) -> impl Iterator<Item = &Weak<SemUndoGroup>> {
        self.undo_groups.iter().map(SemUndoAssociation::group)
    }
}
