use super::*;

#[cfg(feature = "mm_selftest")]
pub(crate) fn run_forced_readonly_debug_selftest() -> Result<String, SystemError> {
    let inner: Arc<dyn FileSystem> = crate::filesystem::ramfs::RamFS::new();
    let mount = MountFS::new(
        inner,
        None,
        None,
        MountPropagation::new_private(),
        None,
        MountFlags::SYNCHRONOUS,
        None,
    )?;
    if let Err(error) = mount.activate() {
        mount.deactivate();
        return Err(error);
    }
    let writer_check = (|| {
        // A filesystem reports the failure while its syscall still owns a
        // writer. Forcing read-only must not wait for that same writer.
        let held = mount.want_write()?;
        let sb = mount.super_block_state();
        sb.force_readonly();
        let preserved_flags = sb
            .flags()
            .contains(MountFlags::RDONLY | MountFlags::SYNCHRONOUS);
        let rejected = matches!(mount.want_write(), Err(SystemError::EROFS));
        drop(held);
        Ok::<_, SystemError>((preserved_flags, rejected))
    })();
    mount.deactivate();
    let (held_writer, new_writer) = writer_check?;

    // This is the actual production transition token, prepared before the
    // backend error and committed afterward with a stale writable flag set.
    let stale = Arc::new(SuperBlockState::new(
        MountFlags::RDONLY | MountFlags::SYNCHRONOUS,
    ));
    let prepared = stale
        .begin_readonly_transition(false, false)?
        .ok_or(SystemError::EIO)?;
    stale.force_readonly();
    prepared.commit(MountFlags::SYNCHRONOUS);
    let late_commit = stale
        .flags()
        .contains(MountFlags::RDONLY | MountFlags::SYNCHRONOUS);
    stale.set_flags(MountFlags::empty());
    let setter = stale.flags() == MountFlags::RDONLY && !stale.is_synchronous();
    let new_rw = matches!(
        stale.begin_readonly_transition(false, false),
        Err(SystemError::EROFS)
    );

    let ok = |value| if value { "ok" } else { "fail" };
    Ok(alloc::format!(
        "status={}\nheld_writer={}\nnew_writer={}\nlate_commit={}\nflags_setter={}\nnew_rw_rejected={}\n",
        ok(held_writer && new_writer && late_commit && setter && new_rw),
        ok(held_writer),
        ok(new_writer),
        ok(late_commit),
        ok(setter),
        ok(new_rw),
    ))
}

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
                .entries
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
            .entries
            .get(&dentry.id)
            .and_then(Weak::upgrade)
            .is_some_and(|cached| Arc::ptr_eq(&cached, &replacement));

    drop(resolved);
    drop(replacement);
    let replacement_final_drop = !mount.wrapper_cache.lock().entries.contains_key(&dentry.id);
    // Real publication/reuse/drop under contention complements the deterministic
    // replacement case above. It observes identity and final cleanup, but does
    // not guarantee every last-drop/replacement scheduler interleaving occurs.
    let concurrent_lookup_identity = run_mount_wrapper_concurrent_lookup_selftest(&mount, &dentry);
    // A contended last drop may leave a dead Weak. The next uncontended
    // publication/release must still recover and retire this exact identity.
    drop(MountFSInode::from_dentry(dentry.clone(), mount.clone()));
    let concurrent_final_drop = mount.wrapper_cache.lock().entries.is_empty();
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

/// Observe completion before releasing a real identity lock. Old blocking
/// implementations are allowed to finish after release, so a negative run
/// reports failure instead of leaving a wedged kernel worker behind.
#[cfg(feature = "mm_selftest")]
fn run_cached_lock_probe<T>(
    held: T,
    operation: impl FnOnce() -> bool + Send + 'static,
    should_finish_while_held: bool,
) -> bool {
    use crate::{
        process::kthread::{KernelThreadClosure, KernelThreadMechanism},
        sched::completion::Completion,
        time::clocksource::HZ,
    };
    use alloc::boxed::Box;

    let entered = Arc::new(Completion::new());
    let completed = Arc::new(Completion::new());
    let worker_entered = entered.clone();
    let worker_completed = completed.clone();
    // KernelThreadClosure is reusable Fn + Sync, but this probe owns one
    // operation (including final-drop fixtures) and executes it exactly once.
    let operation = Mutex::new(Some(operation));
    let closure = KernelThreadClosure::EmptyClosure((
        Box::new(move || {
            worker_entered.complete();
            let operation = operation.lock().take().expect("probe operation runs once");
            let result = operation();
            worker_completed.complete();
            if result {
                0
            } else {
                1
            }
        }),
        (),
    ));
    let Some(worker) =
        KernelThreadMechanism::create_and_run(closure, "cached-lookup-selftest".into())
    else {
        drop(held);
        return false;
    };
    let started =
        matches!(entered.wait_for_completion_timeout((5 * HZ) as i64), Ok(left) if left > 0);
    let finished =
        matches!(completed.wait_for_completion_timeout((HZ / 5) as i64), Ok(left) if left > 0);
    drop(held);
    let joined = matches!(KernelThreadMechanism::stop(&worker), Ok(0));
    started && joined && finished == should_finish_while_held
}

#[cfg(feature = "mm_selftest")]
pub(crate) fn run_cached_lookup_debug_selftest() -> Result<String, SystemError> {
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
    let result = (|| {
        let root = mount.mountpoint_root_inode();
        root.metadata()?; // Initialize the ordinary path's anonymous device.
        for name in ["held", "birth", "churn"] {
            root.dentry
                .inode
                .create(name, FileType::File, InodeMode::S_IRUSR)
                .map_err(|_| SystemError::ENOMEM)?;
        }
        let child = MountFSInode::find_child_from_base(
            root.clone(),
            "held",
            MountLookupPolicy::Backing,
            false,
        )?;
        let cached = MountFSInode::find_child_from_base(
            root.clone(),
            "held",
            MountLookupPolicy::Backing,
            true,
        )?;
        let warm_identity = Arc::ptr_eq(&child, &cached);
        drop(cached);
        let lookup = |root: Arc<MountFSInode>, name: &'static str| {
            move || {
                matches!(
                    MountFSInode::find_child_from_base(
                        root,
                        name,
                        MountLookupPolicy::Backing,
                        true
                    ),
                    Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
                )
            }
        };
        let registry = run_cached_lock_probe(
            mount.super_block_state.dentry_registry.lock(),
            lookup(root.clone(), "held"),
            true,
        );
        let children = run_cached_lock_probe(
            root.dentry.children_gate.lock(),
            lookup(root.clone(), "held"),
            true,
        );
        let namespace = run_cached_lock_probe(
            mount.super_block_state.dentry_namespace_lock.write(),
            lookup(root.clone(), "held"),
            true,
        );
        let state = run_cached_lock_probe(
            child.dentry.state.lock(),
            lookup(root.clone(), "held"),
            true,
        );
        let wrapper = run_cached_lock_probe(
            mount.wrapper_cache.lock(),
            lookup(root.clone(), "held"),
            true,
        );
        // A backing-cache hit with no registered alias must preserve inherited
        // fsnotify state rather than skipping its contended object registry.
        let fsnotify_birth = run_cached_lock_probe(
            mount.super_block_state.fsnotify_object_locks.lock(),
            lookup(root.clone(), "birth"),
            true,
        );
        let metadata_root = root.clone();
        let device = run_cached_lock_probe(
            mount.super_block_state.unnamed_dev_minor.lock(),
            move || {
                matches!(
                    metadata_root.cached_metadata(),
                    Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
                )
            },
            true,
        );
        let metadata_root = root.clone();
        let idmap = run_cached_lock_probe(
            mount.mount_idmap.write(),
            move || {
                matches!(
                    metadata_root.cached_metadata(),
                    Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
                )
            },
            true,
        );
        let ordinary_root = root.clone();
        let ordinary_child = child.clone();
        let ordinary_wait = run_cached_lock_probe(
            mount.super_block_state.dentry_registry.lock(),
            move || {
                MountFSInode::find_child_from_base(
                    ordinary_root,
                    "held",
                    MountLookupPolicy::Backing,
                    false,
                )
                .is_ok_and(|found| Arc::ptr_eq(&found, &ordinary_child))
            },
            false,
        );
        // Weak-only caches must permit both alias and projection rebirth.
        let born = MountFSInode::find_child_from_base(
            root.clone(),
            "birth",
            MountLookupPolicy::Backing,
            true,
        )?;
        let born_id = born.dentry.id;
        let retained_dentry = born.dentry.clone();
        drop(born);
        let projection = MountFSInode::find_child_from_base(
            root.clone(),
            "birth",
            MountLookupPolicy::Backing,
            true,
        )?;
        let projection_rebirth = projection.dentry.id == born_id;
        drop(projection);
        drop(retained_dentry);
        let reborn = MountFSInode::find_child_from_base(
            root.clone(),
            "birth",
            MountLookupPolicy::Backing,
            true,
        )?;
        let alias_rebirth = reborn.dentry.id != born_id;
        drop(reborn);
        let doomed = MountFSInode::find_child_from_base(
            root.clone(),
            "churn",
            MountLookupPolicy::Backing,
            true,
        )?;
        let retired_id = doomed.dentry.id;
        let retirement = run_cached_lock_probe(
            mount.wrapper_cache.lock(),
            move || {
                drop(doomed);
                true
            },
            true,
        );
        let deferred_weak = mount
            .wrapper_cache
            .lock()
            .entries
            .get(&retired_id)
            .is_some_and(|entry| entry.strong_count() == 0);
        // If a counterfactual kernel still blocks final retirement, do not
        // repeat a same-thread held-lock destructor and wedge the test.
        for _ in 0..if retirement {
            2 * MountWrapperCache::SWEEP_INTERVAL
        } else {
            0
        } {
            let born = MountFSInode::find_child_from_base(
                root.clone(),
                "churn",
                MountLookupPolicy::Backing,
                true,
            )?;
            // Deliberately skip destructor cleanup: only births may reclaim.
            let held = mount.wrapper_cache.lock();
            drop(born);
            drop(held);
        }
        let cache = mount.wrapper_cache.lock();
        let dead_weak = cache
            .entries
            .values()
            .filter(|entry| entry.strong_count() == 0)
            .count();
        let periodic_cleanup = retirement && dead_weak <= MountWrapperCache::SWEEP_INTERVAL;
        drop(cache);
        let retry = MountFSInode::find_child_from_base(
            root.clone(),
            "held",
            MountLookupPolicy::Backing,
            true,
        )?;
        let retry_identity = Arc::ptr_eq(&retry, &child);
        let target: Arc<dyn IndexNode> = root.clone();
        root.move_to("held", &target, "renamed", RenameFlags::empty())
            .map_err(|_| SystemError::EIO)?;
        let renamed = MountFSInode::find_child_from_base(
            root.clone(),
            "renamed",
            MountLookupPolicy::Backing,
            true,
        )?;
        let rename_identity = Arc::ptr_eq(&renamed, &child);
        root.unlink("renamed").map_err(|_| SystemError::EIO)?;
        root.dentry
            .inode
            .create("renamed", FileType::File, InodeMode::S_IRUSR)
            .map_err(|_| SystemError::ENOMEM)?;
        let replacement = MountFSInode::find_child_from_base(
            root.clone(),
            "renamed",
            MountLookupPolicy::Backing,
            true,
        )?;
        let namespace_identity = rename_identity
            && child.dentry.is_disconnected()
            && replacement.dentry.id != child.dentry.id;
        let checks = [
            ("warm_identity", warm_identity),
            ("registry", registry),
            ("children", children),
            ("namespace", namespace),
            ("state", state),
            ("wrapper", wrapper),
            ("fsnotify_birth", fsnotify_birth),
            ("device", device),
            ("idmap", idmap),
            ("ordinary_wait", ordinary_wait),
            ("projection_rebirth", projection_rebirth),
            ("alias_rebirth", alias_rebirth),
            ("retirement", retirement),
            ("deferred_weak", deferred_weak),
            ("periodic_cleanup", periodic_cleanup),
            ("retry_identity", retry_identity),
            ("namespace_identity", namespace_identity),
        ];
        let mut report = alloc::format!(
            "status={}\n",
            if checks.iter().all(|(_, ok)| *ok) {
                "ok"
            } else {
                "fail"
            }
        );
        for (name, ok) in checks {
            report.push_str(&alloc::format!(
                "{}={}\n",
                name,
                if ok { "ok" } else { "fail" }
            ));
        }
        Ok(report)
    })();
    mount.deactivate();
    result
}
