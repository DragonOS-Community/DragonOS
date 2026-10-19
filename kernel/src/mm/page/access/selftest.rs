//! Controlled fixtures for the access contract; never move a physical frame.
use super::*;
use crate::{
    arch::MMArch,
    mm::{
        page::{page_manager_lock, PageFlags},
        MemoryManagementArch,
    },
};
use crate::{
    libs::mutex::Mutex,
    mm::{
        remote_access::RemoteAccess,
        syscall::{MapFlags, ProtFlags},
        ucontext::AddressSpace,
    },
    process::{
        kthread::{KernelThreadClosure, KernelThreadMechanism},
        namespace::user_namespace::INIT_USER_NAMESPACE,
        ProcessManager,
    },
    sched::completion::Completion,
    smp::{core::smp_get_processor_id, cpu::smp_cpu_manager},
    time::clocksource::HZ,
};
use alloc::sync::Arc;
use alloc::{
    boxed::Box,
    string::{String, ToString},
};
use core::{
    fmt::Write,
    sync::atomic::{AtomicBool, Ordering},
};
use system_error::SystemError;

static TEST_LOCK: Mutex<()> = Mutex::new(());

struct Fixture {
    mm: Arc<AddressSpace>,
    page: Arc<Page>,
    address: VirtAddr,
}

impl Fixture {
    fn new() -> Result<Self, (&'static str, SystemError)> {
        Self::create(true)
    }

    fn create(populate: bool) -> Result<Self, (&'static str, SystemError)> {
        let mm = AddressSpace::new(false, INIT_USER_NAMESPACE.clone(), 1)
            .map_err(|_| ("new_mm", SystemError::ENOMEM))?;
        let mapping = mm
            .map_anonymous_wait(
                VirtAddr::new(0),
                MMArch::PAGE_SIZE,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS,
                true,
                populate,
            )
            .map_err(|error| ("map", error))?;
        let address = mapping.virt_address();
        if !populate {
            // Exercise the fault-and-pin return, not just the present PTE path.
            mm.access_remote_vm(address.data(), RemoteAccess::Write(&[0]), false)
                .map_err(|error| ("lazy_first_write", error))?;
        }
        let frame = mm
            .read()
            .user_mapper
            .utable
            .translate(address)
            .ok_or(("translate", SystemError::EFAULT))?
            .0;
        let page = page_manager_lock()
            .get_registered(&frame)
            .ok_or(("registry", SystemError::EFAULT))?;
        Ok(Self { mm, page, address })
    }
}

// A try-only API may legitimately encounter an unrelated registry holder.
// Retry only Busy after dropping every MM/Page guard; never retry a failed
// invariant or weaken the deliberate Busy assertions below.
fn isolation_check(
    fixture: &Fixture,
    mut check: impl FnMut(Result<PageIsolationGuard<'_>, PageIsolationError>) -> bool,
) -> bool {
    for _ in 0..64 {
        let outcome = {
            let space = fixture.mm.write();
            let attempt = fixture
                .page
                .try_isolate_for_migration(&space, fixture.address);
            let result = if matches!(attempt, Err(PageIsolationError::Busy)) {
                None
            } else {
                Some(check(attempt))
            };
            result
        };
        if let Some(result) = outcome {
            return result;
        }
        crate::sched::sched_yield();
    }
    false
}

fn identity_and_scope() -> bool {
    let Ok(f) = Fixture::new() else {
        return false;
    };
    let exact = isolation_check(&f, |result| {
        result.is_ok_and(|guard| guard.phys_address() == f.page.phys_address())
    });
    let space = f.mm.write();
    let wrong_address = matches!(
        f.page
            .try_isolate_for_migration(&space, f.address + MMArch::PAGE_SIZE),
        Err(PageIsolationError::StaleMapping)
    );
    drop(space);
    let Ok(other) = Fixture::new() else {
        return false;
    };
    let other_space = other.mm.write();
    let wrong_mm = matches!(
        f.page
            .try_isolate_for_migration(&other_space, other.address),
        Err(PageIsolationError::StaleMapping)
    );
    exact && wrong_address && wrong_mm
}

fn pin_and_isolation() -> bool {
    let Ok(f) = Fixture::new() else {
        return false;
    };
    let space = f.mm.write();
    let Ok(first) = f.page.try_pin_access() else {
        return false;
    };
    let Ok(second) = f.page.try_pin_access() else {
        return false;
    };
    let busy = matches!(
        f.page.try_isolate_for_migration(&space, f.address),
        Err(PageIsolationError::Busy)
    );
    drop(first);
    let still_busy = matches!(
        f.page.try_isolate_for_migration(&space, f.address),
        Err(PageIsolationError::Busy)
    );
    let identity =
        second.phys_address() == f.page.phys_address() && Arc::ptr_eq(second.page(), &f.page);
    drop(second);
    drop(space);
    let excluded = isolation_check(&f, |result| {
        let Ok(isolation) = result else {
            return false;
        };
        let denied = matches!(f.page.try_pin_access(), Err(PageAccessError::Busy));
        let no_second = matches!(
            f.page
                .try_isolate_for_migration(isolation._mm_guard, f.address),
            Err(PageIsolationError::Busy)
        );
        denied && no_second
    });
    busy && still_busy && identity && excluded && f.page.try_pin_access().is_ok()
}

fn data_lock_and_rollback() -> bool {
    let Ok(f) = Fixture::new() else {
        return false;
    };
    let space = f.mm.write();
    let read = f.page.read();
    let reader_blocks = matches!(
        f.page.try_isolate_for_migration(&space, f.address),
        Err(PageIsolationError::Busy)
    );
    drop(read);
    let write = f.page.write();
    let writer_blocks = matches!(
        f.page.try_isolate_for_migration(&space, f.address),
        Err(PageIsolationError::Busy)
    );
    drop(write);
    drop(space);
    for _ in 0..16 {
        if !isolation_check(&f, |result| {
            result
                .is_ok_and(|_isolation| f.page.try_read().is_none() && f.page.try_write().is_none())
        }) || f.page.try_pin_access().is_err()
        {
            return false;
        }
    }
    reader_blocks && writer_blocks
}

fn unsupported_and_revalidation() -> bool {
    let Ok(f) = Fixture::new() else {
        return false;
    };
    let vma = f.mm.read().mappings.contains(f.address).unwrap();
    let flags = *vma.lock().vm_flags();
    {
        let _space = f.mm.write();
        vma.lock().set_vm_flags(flags | crate::mm::VmFlags::VM_IO);
    }
    let special = isolation_check(&f, |result| {
        matches!(result, Err(PageIsolationError::Unsupported(_)))
    });
    {
        let _space = f.mm.write();
        vma.lock().set_vm_flags(flags);
    }
    f.page.write().add_flags(PageFlags::PG_UNEVICTABLE);
    let unevictable = isolation_check(&f, |result| {
        matches!(result, Err(PageIsolationError::Unsupported(_)))
    });
    f.page.write().remove_flags(PageFlags::PG_UNEVICTABLE);
    f.page.write().add_backing_lifetime_pin();
    let backing = isolation_check(&f, |result| {
        matches!(result, Err(PageIsolationError::Unsupported(_)))
    });
    f.page.write().remove_backing_lifetime_pin();
    let mut special_flags = true;
    for flag in [
        PageFlags::PG_RESERVED,
        PageFlags::PG_SLAB,
        PageFlags::PG_HEAD,
        PageFlags::PG_PRIVATE,
    ] {
        f.page.write().add_flags(flag);
        special_flags &= isolation_check(&f, |result| {
            matches!(
                result,
                Err(PageIsolationError::Unsupported(
                    PageIsolationUnsupported::SpecialPageFlags
                ))
            )
        });
        f.page.write().remove_flags(flag);
    }
    special
        && unevictable
        && backing
        && special_flags
        && isolation_check(&f, |result| result.is_ok())
}

fn remote_copy_and_lifetime(report: &mut String) -> bool {
    let f = match Fixture::create(false) {
        Ok(fixture) => fixture,
        Err((stage, error)) => {
            writeln!(report, "remote_lifetime_fixture={stage}:{error:?}").unwrap();
            return false;
        }
    };
    let bytes = [0x31u8; 32];
    let written =
        f.mm.access_remote_vm(f.address.data(), RemoteAccess::Write(&bytes), false);
    if written != Ok(bytes.len()) {
        writeln!(report, "remote_lifetime_write={written:?}").unwrap();
        return false;
    }
    let mut result = [0u8; 32];
    let read =
        f.mm.access_remote_vm(f.address.data(), RemoteAccess::Read(&mut result), false);
    if read != Ok(bytes.len()) || result != bytes {
        writeln!(
            report,
            "remote_lifetime_read={read:?}:equal={}",
            result == bytes
        )
        .unwrap();
        return false;
    }
    use crate::mm::allocator::page_frame::{PageFrameCount, VirtPageFrame};
    let frame = VirtPageFrame::new(f.address);
    let readonly =
        f.mm.mprotect_wait(frame, PageFrameCount::new(1), ProtFlags::PROT_READ);
    if readonly.is_err() {
        writeln!(report, "remote_lifetime_readonly={readonly:?}").unwrap();
        return false;
    }
    let denied =
        f.mm.access_remote_vm(f.address.data(), RemoteAccess::Write(&bytes), false);
    let failed = denied.is_err();
    if !failed {
        writeln!(report, "remote_lifetime_denied={denied:?}").unwrap();
    }
    let writable = f.mm.mprotect_wait(
        frame,
        PageFrameCount::new(1),
        ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
    );
    if writable.is_err() {
        writeln!(report, "remote_lifetime_writable={writable:?}").unwrap();
        return false;
    }
    let drained = isolation_check(&f, |result| result.is_ok());
    if !drained {
        writeln!(
            report,
            "remote_lifetime_drained={drained}:state={}",
            f.page.access_state.load(Ordering::Acquire)
        )
        .unwrap();
    }
    let pin = match f.page.try_pin_access() {
        Ok(pin) => pin,
        Err(error) => {
            writeln!(report, "remote_lifetime_pin={error:?}").unwrap();
            return false;
        }
    };
    let physical = pin.phys_address();
    drop(f);
    let bytes_survive = unsafe { pin.page().read().as_slice()[..bytes.len()] == bytes };
    if !bytes_survive {
        writeln!(report, "remote_lifetime_after_mm_drop={bytes_survive}").unwrap();
    }
    failed && drained && bytes_survive && pin.phys_address() == physical
}

fn pin_overflow() -> bool {
    let Ok(f) = Fixture::new() else {
        return false;
    };
    // Private fixture, no live pins: force the otherwise impractical count
    // boundary, verify admission cannot wrap, then restore before teardown.
    f.page.access_state.store(MAX_PINS, Ordering::Relaxed);
    let rejected = matches!(f.page.try_pin_access(), Err(PageAccessError::Overflow));
    let unchanged = f.page.access_state.load(Ordering::Relaxed) == MAX_PINS;
    f.page.access_state.store(0, Ordering::Release);
    rejected && unchanged && f.page.try_pin_access().is_ok()
}

fn wait(completion: &Completion) -> bool {
    completion
        .wait_for_completion_timeout((5 * HZ) as i64)
        .is_ok_and(|left| left > 0)
}

fn cross_cpu_pin() -> bool {
    let current = smp_get_processor_id();
    let Some(cpu) = smp_cpu_manager()
        .present_cpus()
        .iter_cpu()
        .find(|cpu| *cpu != current && smp_cpu_manager().is_online_cpu(*cpu))
    else {
        return false;
    };
    let Ok(f) = Fixture::new() else {
        return false;
    };
    struct State {
        pinned: Completion,
        release: Completion,
        ok: AtomicBool,
    }
    let state = Arc::new(State {
        pinned: Completion::new(),
        release: Completion::new(),
        ok: AtomicBool::new(false),
    });
    let remote = state.clone();
    let page = f.page.clone();
    let closure = KernelThreadClosure::EmptyClosure((
        Box::new(move || {
            let pin = page.try_pin_access();
            remote.ok.store(
                pin.is_ok() && smp_get_processor_id() == cpu,
                Ordering::Release,
            );
            remote.pinned.complete();
            let released = wait(&remote.release);
            drop(pin);
            i32::from(!released)
        }),
        (),
    ));
    let Some(worker) =
        KernelThreadMechanism::create_on_cpu(closure, "page-access-test".to_string(), cpu)
    else {
        return false;
    };
    if ProcessManager::wakeup(&worker).is_err() {
        state.release.complete();
        let _ = KernelThreadMechanism::stop(&worker);
        return false;
    }
    let arrived = wait(&state.pinned);
    let denied = {
        let space = f.mm.write();
        let result = matches!(
            f.page.try_isolate_for_migration(&space, f.address),
            Err(PageIsolationError::Busy)
        );
        result
    };
    state.release.complete();
    let stopped = matches!(KernelThreadMechanism::stop(&worker), Ok(0));
    let restored = isolation_check(&f, |result| result.is_ok());
    arrived && denied && stopped && restored && state.ok.load(Ordering::Acquire)
}

pub(super) fn run_report() -> String {
    let _serial = TEST_LOCK.lock();
    let mut diagnostics = String::new();
    let cases = [
        ("candidate_identity_and_mm_scope", identity_and_scope()),
        ("pin_isolation_exclusion", pin_and_isolation()),
        ("data_lock_and_rollback", data_lock_and_rollback()),
        ("unsupported_revalidation", unsupported_and_revalidation()),
        (
            "remote_copy_and_pin_lifetime",
            remote_copy_and_lifetime(&mut diagnostics),
        ),
        ("cross_cpu_pin_exclusion", cross_cpu_pin()),
        ("pin_counter_overflow", pin_overflow()),
    ];
    let failed = cases.iter().filter(|(_, ok)| !ok).count();
    let mut report = String::new();
    writeln!(report, "status={}", if failed == 0 { "ok" } else { "fail" }).unwrap();
    for (name, ok) in cases {
        writeln!(report, "{name}={}", if ok { "ok" } else { "fail" }).unwrap();
    }
    writeln!(report, "summary_fail={failed}").unwrap();
    report.push_str(&diagnostics);
    report
}
