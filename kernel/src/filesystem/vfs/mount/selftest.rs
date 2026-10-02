use super::*;

// Every held wrapper must remain the identity returned by real lookups, even
// while another participant creates and drops wrappers for the same dentry.
fn run_mount_wrapper_lookup_churn(mount: &Arc<MountFS>, dentry: &Arc<VfsDentry>) -> bool {
    for _ in 0..4096 {
        let first = MountFSInode::from_dentry(dentry.clone(), mount.clone());
        crate::sched::sched_yield();
        let second = MountFSInode::from_dentry(dentry.clone(), mount.clone());
        if !Arc::ptr_eq(&first, &second) {
            return false;
        }
        drop(first);
        let third = MountFSInode::from_dentry(dentry.clone(), mount.clone());
        if !Arc::ptr_eq(&second, &third)
            || !mount
                .wrapper_cache
                .lock()
                .get(&dentry.id)
                .is_some_and(|cached| Weak::ptr_eq(cached, &Arc::downgrade(&second)))
        {
            return false;
        }
    }
    true
}

fn run_mount_wrapper_concurrent_lookup_selftest(
    mount: &Arc<MountFS>,
    dentry: &Arc<VfsDentry>,
) -> bool {
    use crate::{
        process::kthread::{KernelThreadClosure, KernelThreadMechanism},
        sched::completion::Completion,
    };
    use alloc::boxed::Box;

    let ready = Arc::new(Completion::new());
    let start = Arc::new(Completion::new());
    let identity = Arc::new(Mutex::new(Weak::new()));
    let worker_identity = identity.clone();
    let worker_ready = ready.clone();
    let worker_start = start.clone();
    let worker_mount = mount.clone();
    let worker_dentry = dentry.clone();
    let closure = KernelThreadClosure::EmptyClosure((
        Box::new(move || {
            let held = MountFSInode::from_dentry(worker_dentry.clone(), worker_mount.clone());
            *worker_identity.lock() = Arc::downgrade(&held);
            worker_ready.complete();
            if worker_start.wait_for_completion().is_err() {
                return 1;
            }
            drop(held);
            if run_mount_wrapper_lookup_churn(&worker_mount, &worker_dentry) {
                0
            } else {
                1
            }
        }),
        (),
    ));
    let Some(worker) =
        KernelThreadMechanism::create_and_run(closure, "mount-wrapper-selftest".into())
    else {
        return false;
    };
    let worker_ready = ready.wait_for_completion().is_ok();
    // The worker holds its real lookup result until start is released. Check
    // identity across participants with guaranteed overlapping strong owners.
    let held = MountFSInode::from_dentry(dentry.clone(), mount.clone());
    let shared_identity = Weak::ptr_eq(&identity.lock(), &Arc::downgrade(&held));
    start.complete_all();
    drop(held);
    let local_ok = worker_ready && run_mount_wrapper_lookup_churn(mount, dentry);
    // Join even after a failed local assertion, before inspecting final cache
    // state or deactivating the private mount.
    let worker_ok = matches!(KernelThreadMechanism::stop(&worker), Ok(0));
    local_ok && worker_ok && shared_identity
}

/// Exercise wrapper-cache replacement against the real `MountFSInode::drop`
/// path. Each invocation owns a private mount, so concurrent debugfs readers
/// cannot share cache state or perturb a live namespace.
pub(crate) fn run_mount_wrapper_cache_debug_selftest() -> Result<String, SystemError> {
    let inner: Arc<dyn FileSystem> = crate::filesystem::ramfs::RamFS::new();
    let mount = MountFS::new(
        inner,
        None,
        None,
        MountPropagation::new_private(),
        None,
        MountFlags::empty(),
        None,
    )?;
    if let Err(error) = mount.activate() {
        mount.deactivate();
        return Err(error);
    }

    let old = mount.mountpoint_root_inode();
    let dentry = old.dentry.clone();
    let replacement = Arc::new_cyclic(|self_ref| MountFSInode {
        dentry: dentry.clone(),
        mount_fs: mount.clone(),
        self_ref: self_ref.clone(),
    });
    mount
        .wrapper_cache
        .lock()
        .insert(dentry.id, Arc::downgrade(&replacement));

    // This is the exact state reached when lookup replaces an expired Weak
    // before the old wrapper's destructor obtains wrapper_cache. Dropping the
    // old object now invokes the production destructor; an unconditional
    // removal would erase `replacement` and fail the lookup identity check.
    drop(old);
    let resolved = mount.mountpoint_root_inode();
    let replacement_survives_old_drop = Arc::ptr_eq(&resolved, &replacement)
        && mount
            .wrapper_cache
            .lock()
            .get(&dentry.id)
            .and_then(Weak::upgrade)
            .is_some_and(|cached| Arc::ptr_eq(&cached, &replacement));

    drop(resolved);
    drop(replacement);
    let replacement_final_drop = !mount.wrapper_cache.lock().contains_key(&dentry.id);
    // Real publication/reuse/drop under contention complements the deterministic
    // replacement case above. It observes identity and final cleanup, but does
    // not guarantee every last-drop/replacement scheduler interleaving occurs.
    let concurrent_lookup_identity = run_mount_wrapper_concurrent_lookup_selftest(&mount, &dentry);
    let concurrent_final_drop = mount.wrapper_cache.lock().is_empty();
    mount.deactivate();

    Ok(alloc::format!(
        "status={}\nreplacement_survives_old_drop={}\nreplacement_final_drop={}\nconcurrent_lookup_identity={}\nconcurrent_final_drop={}\n",
        if replacement_survives_old_drop && replacement_final_drop
            && concurrent_lookup_identity && concurrent_final_drop {
            "ok"
        } else {
            "fail"
        },
        if replacement_survives_old_drop {
            "ok"
        } else {
            "fail"
        },
        if replacement_final_drop { "ok" } else { "fail" },
        if concurrent_lookup_identity { "ok" } else { "fail" },
        if concurrent_final_drop { "ok" } else { "fail" },
    ))
}
