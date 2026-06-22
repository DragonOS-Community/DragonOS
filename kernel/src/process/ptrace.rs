use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

use super::{
    abi::WaitOption, ProcessControlBlock, ProcessFlags, ProcessManager, RawPid,
    PTRACE_RELATION_LOCK,
};

fn traceme_allowed(
    parent: &Arc<ProcessControlBlock>,
    child: &Arc<ProcessControlBlock>,
) -> Result<(), SystemError> {
    if is_ptraced_locked(child) {
        return Err(SystemError::EPERM);
    }
    if parent.flags().contains(ProcessFlags::EXITING) {
        return Err(SystemError::EPERM);
    }

    // Linux also calls security_ptrace_traceme() here. DragonOS does not yet
    // have the equivalent LSM/dumpable/credential/capability hooks wired into
    // ptrace, so keep this as the single future extension point instead of
    // spreading partial checks across syscall and wait code.
    Ok(())
}

fn traceme_parent_for(
    child: &Arc<ProcessControlBlock>,
) -> Result<Arc<ProcessControlBlock>, SystemError> {
    let real_parent = child.real_parent_pcb().ok_or(SystemError::EPERM)?;
    let Some(fork_parent) = child.fork_parent_pcb() else {
        return Ok(real_parent);
    };

    if fork_parent.tgid == real_parent.tgid {
        Ok(fork_parent)
    } else {
        Ok(real_parent)
    }
}

pub fn traceme_current() -> Result<(), SystemError> {
    let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
    let current = ProcessManager::current_pcb();
    let tracer = traceme_parent_for(&current)?;
    traceme_allowed(&tracer, &current)?;

    let raw_pid = current.raw_pid();
    {
        let mut ptracer = current.ptracer_pcb.write_irqsave();
        if ptracer.upgrade().is_some() {
            return Err(SystemError::EPERM);
        }
        *ptracer = Arc::downgrade(&tracer);
        current.flags().insert(ProcessFlags::PTRACED);
    }

    let mut ptraced = tracer.ptraced.write_irqsave();
    if !ptraced.contains(&raw_pid) {
        ptraced.push(raw_pid);
    }

    Ok(())
}

pub fn unlink_tracee(tracee: &Arc<ProcessControlBlock>) {
    let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
    let tracer = {
        let mut ptracer = tracee.ptracer_pcb.write_irqsave();
        let tracer = ptracer.upgrade();
        *ptracer = Weak::new();
        tracee.flags().remove(ProcessFlags::PTRACED);
        tracer
    };

    if let Some(tracer) = tracer {
        let raw_pid = tracee.raw_pid();
        tracer.ptraced.write_irqsave().retain(|pid| *pid != raw_pid);
    }
}

pub fn exit_ptrace(tracer: &Arc<ProcessControlBlock>) {
    let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
    let traced_pids: Vec<RawPid> = {
        let mut ptraced = tracer.ptraced.write_irqsave();
        core::mem::take(&mut *ptraced)
    };

    for pid in traced_pids {
        let Some(tracee) = ProcessManager::find(pid) else {
            continue;
        };
        {
            let mut ptracer = tracee.ptracer_pcb.write_irqsave();
            if ptracer
                .upgrade()
                .as_ref()
                .map(|t| Arc::ptr_eq(t, tracer))
                .unwrap_or(false)
            {
                *ptracer = Weak::new();
                tracee.flags().remove(ProcessFlags::PTRACED);
            }
        }
        if let Some(real_parent) = tracee.real_parent_pcb() {
            real_parent
                .wait_queue
                .wakeup_all(Some(super::ProcessState::Blocked(true)));
        }
    }
}

pub fn tracees_of(tracer: &Arc<ProcessControlBlock>) -> Vec<RawPid> {
    let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
    tracees_of_locked(tracer)
}

fn tracees_of_locked(tracer: &Arc<ProcessControlBlock>) -> Vec<RawPid> {
    tracer.ptraced.read_irqsave().clone()
}

pub fn ptracer_of(tracee: &Arc<ProcessControlBlock>) -> Option<Arc<ProcessControlBlock>> {
    let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
    ptracer_of_locked(tracee)
}

fn ptracer_of_locked(tracee: &Arc<ProcessControlBlock>) -> Option<Arc<ProcessControlBlock>> {
    tracee.ptracer_pcb.read_irqsave().upgrade()
}

pub fn is_ptraced(tracee: &ProcessControlBlock) -> bool {
    let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
    is_ptraced_locked(tracee)
}

fn is_ptraced_locked(tracee: &ProcessControlBlock) -> bool {
    tracee.flags().contains(ProcessFlags::PTRACED)
        && tracee.ptracer_pcb.read_irqsave().upgrade().is_some()
}

pub fn is_wait_tracee_of(
    tracee: &Arc<ProcessControlBlock>,
    waiter: &Arc<ProcessControlBlock>,
    options: WaitOption,
) -> bool {
    let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
    let Some(tracer) = ptracer_of_locked(tracee) else {
        return false;
    };

    let same_waiter = Arc::ptr_eq(&tracer, waiter);
    let same_thread_group = !options.contains(WaitOption::WNOTHREAD) && tracer.tgid == waiter.tgid;
    if !same_waiter && !same_thread_group {
        return false;
    }

    tracees_of_locked(&tracer).contains(&tracee.raw_pid())
}
