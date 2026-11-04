//! Explicitly enabled credential regression, never a production debug entry.
use alloc::{boxed::Box, format, string::String, string::ToString, sync::Arc};
use system_error::SystemError;

use crate::{
    arch::CurrentIrqArch,
    debug::sysfs::debugfs_kobj,
    driver::base::kobject::KObject,
    exception::InterruptArch,
    filesystem::{
        kernfs::callback::{KernCallbackData, KernFSCallback, KernFilePrivateData},
        vfs::{InodeMode, PollStatus},
    },
    process::{
        kthread::{KernelThreadClosure, KernelThreadMechanism},
        Cred, ProcessManager,
    },
    sched::completion::Completion,
};

#[derive(Debug)]
struct CredentialSelftest;

static SELFTEST: CredentialSelftest = CredentialSelftest;

impl KernFSCallback for CredentialSelftest {
    fn open(&self, mut data: KernCallbackData) -> Result<(), SystemError> {
        let report = run_selftests()?;
        data.file_private_data_mut()
            .replace(KernFilePrivateData::DebugTextSnapshot(report));
        Ok(())
    }

    fn read(
        &self,
        data: KernCallbackData,
        buf: &mut [u8],
        offset: usize,
    ) -> Result<usize, SystemError> {
        let Some(KernFilePrivateData::DebugTextSnapshot(report)) = data.file_private_data() else {
            return Err(SystemError::EINVAL);
        };
        let bytes = report.as_bytes();
        if offset >= bytes.len() {
            return Ok(0);
        }
        let len = buf.len().min(bytes.len() - offset);
        buf[..len].copy_from_slice(&bytes[offset..offset + len]);
        Ok(len)
    }

    fn write(
        &self,
        _data: KernCallbackData,
        _buf: &[u8],
        _offset: usize,
    ) -> Result<usize, SystemError> {
        Err(SystemError::EPERM)
    }

    fn poll(&self, _data: KernCallbackData) -> Result<PollStatus, SystemError> {
        Ok(PollStatus::READ)
    }
}

fn run_selftests() -> Result<String, SystemError> {
    let pcb = ProcessManager::current_pcb();
    let objective = pcb.objective_cred();
    // Match the owner-only inode mode even if a caller could bypass DAC.
    if objective.euid.data() != 0 {
        return Err(SystemError::EPERM);
    }
    if !Arc::ptr_eq(&ProcessManager::current_cred(), &objective)
        || crate::exception::in_interrupt()
        || pcb.preempt_count() != 0
        || !CurrentIrqArch::is_irq_enabled()
    {
        return Err(SystemError::EBUSY);
    }

    // Only private credential copies change; no formal publication or UID change.
    let mut first_value = (*objective).clone();
    first_value.keepcaps = !objective.keepcaps;
    let first = Cred::try_new_arc(first_value)?;
    let second = Cred::try_new_arc((*objective).clone())?;

    let objective_window = ProcessManager::with_current_credentials(first.clone(), || {
        Arc::ptr_eq(&ProcessManager::current_cred(), &first)
            && ProcessManager::current_cred().keepcaps == first.keepcaps
            && Arc::ptr_eq(&pcb.objective_cred(), &objective)
    }) && Arc::ptr_eq(&ProcessManager::current_cred(), &objective);

    let nested_distinct = ProcessManager::with_current_credentials(first.clone(), || {
        let inner = ProcessManager::with_current_credentials(second.clone(), || {
            Arc::ptr_eq(&ProcessManager::current_cred(), &second)
                && Arc::ptr_eq(&pcb.objective_cred(), &objective)
        });
        inner && Arc::ptr_eq(&ProcessManager::current_cred(), &first)
    }) && Arc::ptr_eq(&ProcessManager::current_cred(), &objective);

    let nested_same = ProcessManager::with_current_credentials(first.clone(), || {
        let inner = ProcessManager::with_current_credentials(first.clone(), || {
            Arc::ptr_eq(&ProcessManager::current_cred(), &first)
                && Arc::ptr_eq(&pcb.objective_cred(), &objective)
        });
        inner && Arc::ptr_eq(&ProcessManager::current_cred(), &first)
    }) && Arc::ptr_eq(&ProcessManager::current_cred(), &objective);

    let error: Result<(), SystemError> =
        ProcessManager::with_current_credentials(first.clone(), || Err(SystemError::EINVAL));
    let result_error_restore = error == Err(SystemError::EINVAL)
        && Arc::ptr_eq(&ProcessManager::current_cred(), &objective);

    // The returned Arc models an owned credential snapshot, not a borrowed guard.
    // Weak expiration after dropping it proves the override slot retained no owner.
    let ephemeral = Cred::try_new_arc((*objective).clone())?;
    let weak = Arc::downgrade(&ephemeral);
    let captured =
        ProcessManager::with_current_credentials(ephemeral, ProcessManager::current_cred);
    let captured_live = weak.upgrade().is_some()
        && !Arc::ptr_eq(&captured, &objective)
        && Arc::ptr_eq(&ProcessManager::current_cred(), &objective);
    drop(captured);
    let captured_snapshot_lifetime = captured_live && weak.upgrade().is_none();

    let irq_disabled_read = ProcessManager::with_current_credentials(first.clone(), || {
        let before = CurrentIrqArch::is_irq_enabled();
        let observed = {
            let _irq = unsafe { CurrentIrqArch::save_and_disable_irq() };
            !CurrentIrqArch::is_irq_enabled()
                && Arc::ptr_eq(&ProcessManager::current_cred(), &first)
        };
        observed && CurrentIrqArch::is_irq_enabled() == before
    });

    let task_isolation = task_isolation(&first, &objective)?;

    let final_restore = Arc::ptr_eq(&ProcessManager::current_cred(), &objective)
        && Arc::ptr_eq(&pcb.objective_cred(), &objective)
        && CurrentIrqArch::is_irq_enabled();
    let cases = [
        ("objective_window", objective_window),
        ("nested_distinct", nested_distinct),
        ("nested_same", nested_same),
        ("result_error_restore", result_error_restore),
        ("captured_snapshot_lifetime", captured_snapshot_lifetime),
        ("irq_disabled_read", irq_disabled_read),
        ("task_isolation", task_isolation),
        ("final_restore", final_restore),
    ];
    let failed = cases.iter().filter(|(_, passed)| !passed).count();
    let mut report = format!(
        "status={}\npassed={}\nfailed={}\n",
        if failed == 0 { "ok" } else { "fail" },
        cases.len() - failed,
        failed
    );
    for (name, passed) in cases {
        report.push_str(&format!("{name}={}\n", if passed { "ok" } else { "fail" }));
    }
    report.push_str("migration=not_proven\n");
    Ok(report)
}

// The worker exists before the caller installs an override. The handshake
// tests two real tasks, without assuming that schedule() switched tasks or
// that either task migrated. Every return joins the worker after restoring
// the caller's credentials.
fn task_isolation(first: &Arc<Cred>, objective: &Arc<Cred>) -> Result<bool, SystemError> {
    const TIMEOUT: i64 = 5 * crate::time::clocksource::HZ as i64;
    let start = Arc::try_new(Completion::new()).map_err(|_| SystemError::ENOMEM)?;
    let done = Arc::try_new(Completion::new()).map_err(|_| SystemError::ENOMEM)?;
    let start_worker = start.clone();
    let done_worker = done.clone();
    let owner = ProcessManager::current_pcb();
    let owner_worker = owner.clone();
    let objective_worker = objective.clone();
    let worker = Box::try_new(move || {
        let result = (|| -> Result<bool, SystemError> {
            if start_worker.wait_for_completion_timeout(TIMEOUT)? == 0 {
                return Ok(false);
            }
            let pcb = ProcessManager::current_pcb();
            let original = pcb.objective_cred();
            let mut value = (*original).clone();
            value.keepcaps = !original.keepcaps;
            let temporary = Cred::try_new_arc(value)?;
            let isolated = !Arc::ptr_eq(&pcb, &owner_worker)
                && Arc::ptr_eq(&ProcessManager::current_cred(), &original)
                && Arc::ptr_eq(&owner_worker.objective_cred(), &objective_worker);
            let nested = ProcessManager::with_current_credentials(temporary.clone(), || {
                Arc::ptr_eq(&ProcessManager::current_cred(), &temporary)
                    && Arc::ptr_eq(&pcb.objective_cred(), &original)
            });
            Ok(isolated && nested && Arc::ptr_eq(&ProcessManager::current_cred(), &original))
        })();
        done_worker.complete();
        if result == Ok(true) {
            0
        } else {
            1
        }
    })
    .map_err(|_| SystemError::ENOMEM)?;
    let pcb = KernelThreadMechanism::create_and_run(
        KernelThreadClosure::EmptyClosure((worker, ())),
        "credential-selftest".to_string(),
    )
    .ok_or(SystemError::ENOMEM)?;
    let caller = ProcessManager::with_current_credentials(first.clone(), || {
        start.complete();
        done.wait_for_completion_timeout(TIMEOUT)
            .is_ok_and(|left| left > 0)
            && Arc::ptr_eq(&ProcessManager::current_cred(), first)
            && Arc::ptr_eq(&owner.objective_cred(), objective)
    });
    let worker_result = KernelThreadMechanism::stop(&pcb);
    Ok(caller && worker_result == Ok(0))
}

pub(crate) fn init_debugfs_credentials() -> Result<(), SystemError> {
    let root = debugfs_kobj().inode().ok_or(SystemError::ENOENT)?;
    let directory = root.add_dir(
        "credentials".to_string(),
        InodeMode::from_bits_truncate(0o500),
        None,
        None,
    )?;
    directory.add_file(
        "selftest".to_string(),
        InodeMode::S_IRUSR,
        Some(4096),
        None,
        Some(&SELFTEST),
    )?;
    Ok(())
}
