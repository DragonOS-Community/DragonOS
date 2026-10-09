//! Deterministic ownership checks for the existing process-lifecycle debug test.
//! These tasks are never scheduled or published in a running PID namespace.

use super::*;
use crate::process::KernelStack;

fn task() -> Result<Arc<ProcessControlBlock>, SystemError> {
    let task =
        ProcessControlBlock::new("pid-lifetime-selftest".into(), KernelStack::new()?, false)?;
    // PCB::new inherits a real parent; this fixture must not alter that
    // parent's children on Drop, including after a test PID has been reused.
    *task.parent_pcb.write() = Weak::new();
    *task.real_parent_pcb.write() = Weak::new();
    *task.wait_parent_pcb.write() = Weak::new();
    *task.fork_parent_pcb.write() = Weak::new();
    task.sig_info_mut().set_tty(None);
    Ok(task)
}

fn install_private_pid(task: &ProcessControlBlock, pid: Arc<Pid>, ns: &Arc<PidNamespace>) {
    task.pid.store(pid.pid_nr_ns(ns), Ordering::Release);
    task.init_task_pid(PidType::PID, pid);
}

fn retired(ns: &Arc<PidNamespace>, pid: &Arc<Pid>) -> bool {
    PidType::ALL.iter().all(|ty| !pid.has_task(*ty))
        && pid
            .registered
            .iter()
            .all(|bit| !bit.load(Ordering::Acquire))
        && ns.find_pid_in_ns(pid.pid_nr_ns(ns)).is_none()
}

fn group_lifetime(leader_first: bool) -> Result<bool, SystemError> {
    let leader = task()?;
    let member = task()?;
    let unpublished = task()?;
    let ns = PidNamespace::new_root();
    let leader_pid = alloc_pid(&ns)?;
    install_private_pid(&leader, leader_pid.clone(), &ns);
    // Installing the private identity before the next fallible allocation
    // preserves normal PCB Drop rollback if allocating the member fails.
    let member_pid = alloc_pid(&ns)?;
    install_private_pid(&member, member_pid.clone(), &ns);
    leader.init_task_pid(PidType::TGID, leader_pid.clone());
    member.replace_process_signal(leader.process_signal());
    unpublished.replace_process_signal(leader.process_signal());
    member.exit_signal.store(-1, Ordering::Release);
    unpublished.exit_signal.store(-1, Ordering::Release);
    let baseline_weak = Arc::weak_count(&member);

    for ty in [PidType::PID, PidType::TGID] {
        leader.attach_pid(ty);
        member.attach_pid(ty);
    }
    // A failed, unpublished thread must not detach another task's shared
    // logical TGID. Also exercise a second cleanup through PCB::drop.
    unpublished.__exit_signal();
    drop(unpublished);
    let mut ok = leader_pid.tasks[PidType::TGID as usize]
        .lock_irqsave()
        .len()
        == 2;

    if leader_first {
        leader.__exit_signal();
        ok &= ns.find_pid_in_ns(leader_pid.pid_nr_ns(&ns)).is_some()
            && leader_pid.tasks[PidType::TGID as usize]
                .lock_irqsave()
                .len()
                == 1;
    }
    member.__exit_signal();
    ok &= member.pid_links[PidType::PID as usize].pid.read().is_none()
        && member.pid_links[PidType::TGID as usize]
            .pid
            .read()
            .is_none()
        && Arc::weak_count(&member) == baseline_weak
        && retired(&ns, &member_pid)
        && member
            .task_pid_ptr(PidType::TGID)
            .is_some_and(|pid| Arc::ptr_eq(&pid, &leader_pid));
    member.__exit_signal();
    ok &= Arc::weak_count(&member) == baseline_weak;
    if !leader_first {
        ok &= leader_pid.tasks[PidType::TGID as usize]
            .lock_irqsave()
            .len()
            == 1
            && ns.find_pid_in_ns(leader_pid.pid_nr_ns(&ns)).is_some();
        leader.__exit_signal();
    }
    ok &= retired(&ns, &leader_pid);

    // Always clean fixtures, even when run against the broken exit path.
    // A failing selftest must not itself leave namespace registration cycles.
    member.detach_pid(PidType::TGID);
    leader.detach_pid(PidType::TGID);
    free_pid(member_pid);
    free_pid(leader_pid);
    Ok(ok)
}

pub(crate) fn selftest_membership_lifetime() -> Result<bool, SystemError> {
    let member_first = group_lifetime(false)?;
    let leader_first = group_lifetime(true)?;

    let unpublished = task()?;
    let ns = PidNamespace::new_root();
    let pid = alloc_pid(&ns)?;
    install_private_pid(&unpublished, pid.clone(), &ns);
    // Models fork failing after alloc_pid but before attach_pid(PID).
    drop(unpublished);
    let rollback = retired(&ns, &pid);
    free_pid(pid);
    Ok(member_first && leader_first && rollback)
}
