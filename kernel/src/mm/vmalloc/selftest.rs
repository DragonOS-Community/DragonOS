//! Controlled tests for the explicit RAM allocator; never exhaust guest RAM.

use super::*;
use crate::mm::allocation::MemoryAllocationError;
use alloc::{
    boxed::Box,
    string::{String, ToString},
    sync::Arc,
};
use core::sync::atomic::{AtomicBool, Ordering};
use core::{alloc::Layout, fmt::Write};

use crate::{
    cgroup::memory::{ChargeFailureReason, MemoryAccount, MemoryPolicy},
    libs::mutex::Mutex,
    process::{
        kthread::{KernelThreadClosure, KernelThreadMechanism},
        preempt::PreemptGuard,
    },
    sched::completion::Completion,
    smp::{core::smp_get_processor_id, cpu::smp_cpu_manager},
    time::{clocksource::HZ, timer::clock},
};

static TEST_LOCK: Mutex<()> = Mutex::new(());

fn used_pages() -> usize {
    POOL.lock_irqsave()
        .used
        .iter()
        .map(|word| word.count_ones() as usize)
        .sum()
}

fn basic() -> bool {
    let layout =
        Layout::from_size_align(3 * MMArch::PAGE_SIZE + 17, 2 * MMArch::PAGE_SIZE).unwrap();
    VmAllocation::try_new_zeroed(layout).is_ok_and(|mut allocation| {
        let zero = allocation.as_slice().iter().all(|byte| *byte == 0);
        let aligned = allocation.address().check_aligned(layout.align());
        for (index, byte) in allocation.as_mut_slice().iter_mut().enumerate() {
            *byte = index as u8;
        }
        zero && allocation.len() == layout.size()
            && aligned
            && allocation
                .as_slice()
                .iter()
                .enumerate()
                .all(|(index, byte)| *byte == index as u8)
            && unsafe { MMArch::virt_2_phys(allocation.address()) }.is_none()
    })
}

fn rollback() -> bool {
    let layout = Layout::from_size_align(65 * MMArch::PAGE_SIZE, 2 << 20).unwrap();
    let baseline = used_pages();
    let Ok(account) = MemoryAccount::new(u64::MAX - 1, None) else {
        return false;
    };
    let Ok(prepared) = ObjectAccount::prepare(&account) else {
        return false;
    };
    // Two nodes, one table, and two failure points per data page fit below this bound.
    // Stop only when the injected transaction actually reaches success.
    for step in 0..256 {
        let result = VmAllocation::allocate_owned(
            layout,
            65,
            &mut FailureBudget(Some(step)),
            &mut frame_zeroed,
            prepared.object_account(),
        );
        let succeeded = result.is_ok();
        if result.as_ref().is_err_and(|error| {
            !matches!(error, MemoryAllocationError::System(SystemError::ENOMEM))
        }) {
            return false;
        }
        drop(result);
        if used_pages() != baseline
            || account.current_pages() != 0
            || account
                .snapshot()
                .kernel_bytes
                .iter()
                .any(|bytes| *bytes != 0)
        {
            return false;
        }
        if succeeded {
            return true;
        }
    }
    false
}

struct OddPages {
    frames: [PhysAddr; 4],
    count: usize,
}

impl Drop for OddPages {
    fn drop(&mut self) {
        for frame in &self.frames[..self.count] {
            unsafe { LockedFrameAllocator.free_one(*frame) };
        }
    }
}

fn noncontiguous() -> bool {
    let mut odd = OddPages {
        frames: [PhysAddr::new(0); 4],
        count: 0,
    };
    let layout = Layout::from_size_align(4 * MMArch::PAGE_SIZE, MMArch::PAGE_SIZE).unwrap();
    let allocation = VmAllocation::allocate_with(layout, &mut FailureBudget(None), &mut || {
        let (even, count) = unsafe {
            LockedFrameAllocator.allocate_below(PageFrameCount::new(2), PhysAddr::new(usize::MAX))
        }
        .ok_or(SystemError::ENOMEM)?;
        assert_eq!(count, PageFrameCount::new(2));
        // Splitting a globally allocated order-1 run into two order-0 owners is
        // supported by buddy. Retain every odd page until the buffer is destroyed.
        odd.frames[odd.count] = even + MMArch::PAGE_SIZE;
        odd.count += 1;
        let address = unsafe { MMArch::phys_2_virt(even) }.unwrap();
        unsafe { core::ptr::write_bytes(address.as_ptr::<u8>(), 0, MMArch::PAGE_SIZE) };
        Ok(even)
    });
    let Ok(mut allocation) = allocation else {
        return false;
    };
    let Some(chunk) = allocation.chunks.as_ref() else {
        return false;
    };
    let frames = &chunk.frames[..chunk.acquired];
    let separated = frames.len() == 4
        && frames
            .windows(2)
            .all(|pair| pair[0].data().abs_diff(pair[1].data()) >= 2 * MMArch::PAGE_SIZE);
    allocation.as_mut_slice().fill(0x73);
    separated && allocation.as_slice().iter().all(|byte| *byte == 0x73)
}

fn shared_table() -> bool {
    let layout = Layout::from_size_align(MMArch::PAGE_SIZE, MMArch::PAGE_SIZE).unwrap();
    let crossing = Layout::from_size_align(
        (TABLE_PAGES + 1) * MMArch::PAGE_SIZE,
        TABLE_PAGES * MMArch::PAGE_SIZE,
    )
    .unwrap();
    let Ok(mut first) = VmAllocation::try_new_zeroed(crossing) else {
        return false;
    };
    first.as_mut_slice().fill(0x42);
    if !first.as_slice().iter().all(|byte| *byte == 0x42) {
        return false;
    }
    let Ok(mut second) = VmAllocation::try_new_zeroed(layout) else {
        return false;
    };
    let slot = (first.start + first.pages - 1) / TABLE_PAGES;
    if slot != second.start / TABLE_PAGES {
        return false;
    }
    let Some(table_frame) = POOL.lock_irqsave().tables[slot]
        .as_ref()
        .map(|table| table.frame)
    else {
        return false;
    };
    second.as_mut_slice().fill(0x95);
    drop(first);
    let retained = POOL.lock_irqsave().tables[slot]
        .as_ref()
        .is_some_and(|table| table.frame == table_frame);
    let intact = second.as_slice().iter().all(|byte| *byte == 0x95);
    drop(second);
    let reclaimed = POOL.lock_irqsave().tables[slot].is_none();
    retained && intact && reclaimed && concurrent_allocations()
}

fn concurrent_allocations() -> bool {
    let initial_cpu = smp_get_processor_id();
    let Some(cpu) = smp_cpu_manager()
        .present_cpus()
        .iter_cpu()
        .find(|cpu| *cpu != initial_cpu && smp_cpu_manager().is_online_cpu(*cpu))
    else {
        return false;
    };
    let baseline = used_pages();
    let ready = Arc::new(Completion::new());
    let start = Arc::new(Completion::new());
    let remote_ready = ready.clone();
    let remote_start = start.clone();
    let layout = Layout::from_size_align(17 * MMArch::PAGE_SIZE, MMArch::PAGE_SIZE).unwrap();
    let closure = KernelThreadClosure::EmptyClosure((
        Box::new(move || {
            let first = VmAllocation::try_new_zeroed(layout);
            remote_ready.complete();
            if !wait(&remote_start) {
                return 1;
            }
            let Ok(mut allocation) = first else {
                return 1;
            };
            for _ in 0..32 {
                allocation.as_mut_slice().fill(0x58);
                crate::sched::sched_yield();
                if !allocation.as_slice().iter().all(|byte| *byte == 0x58) {
                    return 1;
                }
                drop(allocation);
                let Ok(next) = VmAllocation::try_new_zeroed(layout) else {
                    return 1;
                };
                allocation = next;
            }
            0
        }),
        (),
    ));
    let Some(worker) =
        KernelThreadMechanism::create_on_cpu(closure, "vmalloc-race-test".to_string(), cpu)
    else {
        return false;
    };
    let woke = ProcessManager::wakeup(&worker).is_ok();
    let initialized = woke && wait(&ready);
    // Both first allocations exist before either side starts recycling them.
    let mut first = initialized
        .then(|| VmAllocation::try_new_zeroed(layout))
        .and_then(Result::ok);
    start.complete();
    let mut intact = initialized && first.is_some();
    if intact {
        for _ in 0..32 {
            let Some(mut allocation) = first.take() else {
                intact = false;
                break;
            };
            allocation.as_mut_slice().fill(0xb7);
            crate::sched::sched_yield();
            intact &= allocation.as_slice().iter().all(|byte| *byte == 0xb7);
            drop(allocation);
            first = VmAllocation::try_new_zeroed(layout).ok();
        }
    }
    intact &= first.is_some();
    drop(first);
    let joined = matches!(KernelThreadMechanism::stop(&worker), Ok(0));
    intact && joined && used_pages() == baseline
}

fn boundaries() -> bool {
    let baseline = used_pages();
    let zero = matches!(
        VmAllocation::try_new_zeroed(Layout::from_size_align(0, 1).unwrap()),
        Err(MemoryAllocationError::System(SystemError::EINVAL))
    );
    let too_large = matches!(
        VmAllocation::try_new_zeroed(Layout::from_size_align(SIZE + 1, 1).unwrap()),
        Err(MemoryAllocationError::System(SystemError::ENOMEM))
    );
    let too_aligned = matches!(
        VmAllocation::try_new_zeroed(Layout::from_size_align(1, SIZE * 2).unwrap()),
        Err(MemoryAllocationError::System(SystemError::ENOMEM))
    );
    let huge = matches!(
        VmAllocation::try_new_zeroed(Layout::from_size_align(isize::MAX as usize, 1).unwrap()),
        Err(MemoryAllocationError::System(SystemError::ENOMEM))
    );
    zero && too_large && too_aligned && huge && used_pages() == baseline
}

fn accounting() -> bool {
    let Ok(account) = MemoryAccount::new(u64::MAX, None) else {
        return false;
    };
    // Unpublished owner: no global registry node and no asynchronous accounting users.
    let Ok(prepared) = ObjectAccount::prepare(&account) else {
        return false;
    };
    let owner = prepared.object_account();
    let pages = TABLE_PAGES;
    let bytes = pages * MMArch::PAGE_SIZE;
    let layout = Layout::from_size_align(bytes, bytes).unwrap();
    let Ok(allocation) = VmAllocation::allocate_owned(
        layout,
        pages,
        &mut FailureBudget(None),
        &mut frame_zeroed,
        owner,
    ) else {
        return false;
    };
    let snapshot = account.snapshot();
    let metadata = (pages.div_ceil(CHUNK) as u64)
        * crate::mm::allocator::kernel_allocator::allocation_footprint(Layout::new::<Chunk>());
    let expected_bytes = bytes as u64 + MMArch::PAGE_SIZE as u64 + metadata;
    let charged = snapshot.kernel_bytes[KernelByteKind::Vmalloc as usize] == bytes as u64
        && snapshot.kernel_bytes[KernelByteKind::PageTables as usize] == MMArch::PAGE_SIZE as u64
        && snapshot.kernel_bytes[KernelByteKind::SlabUnreclaimable as usize] == metadata
        && snapshot.current == expected_bytes.div_ceil(MMArch::PAGE_SIZE as u64);
    drop(allocation);
    let empty = account.snapshot();
    charged
        && empty.current == 0
        && empty.kernel_bytes.iter().all(|bytes| *bytes == 0)
        && admission_failures(&account, layout, pages)
}

fn admission_failures(root: &Arc<MemoryAccount>, layout: Layout, pages: usize) -> bool {
    let baseline = used_pages();
    // Max=0 rejects backing; exactly backing rejects the first metadata node;
    // backing + one metadata page admits the first node and rejects its table.
    for (limit, expected_data_calls) in [(0, 0), (pages as u64, 0), (pages as u64 + 1, 1)] {
        let Ok(child) = MemoryAccount::new(u64::MAX - 2, Some(root.clone())) else {
            return false;
        };
        child.set_policy(MemoryPolicy {
            max: Some(limit),
            ..MemoryPolicy::default()
        });
        let Ok(prepared) = ObjectAccount::prepare(&child) else {
            return false;
        };
        let mut data_calls = 0;
        let result = VmAllocation::allocate_owned(
            layout,
            pages,
            &mut FailureBudget(None),
            &mut || {
                data_calls += 1;
                frame_zeroed()
            },
            prepared.object_account(),
        );
        let rejected = matches!(&result,
            Err(MemoryAllocationError::Charge(failure))
                if failure.reason == ChargeFailureReason::Max && Arc::ptr_eq(&failure.account, &child));
        drop(result);
        if !rejected
            || data_calls != expected_data_calls
            || used_pages() != baseline
            || child.current_pages() != 0
            || root.current_pages() != 0
            || child
                .snapshot()
                .kernel_bytes
                .iter()
                .any(|bytes| *bytes != 0)
        {
            return false;
        }
    }
    true
}

struct Visibility {
    first_read: Completion,
    read_replacement: Completion,
    replacement_ready: AtomicBool,
    abort: AtomicBool,
    first_ok: AtomicBool,
    second_ok: AtomicBool,
}

fn wait(completion: &Completion) -> bool {
    completion
        .wait_for_completion_timeout((5 * HZ) as i64)
        .is_ok_and(|left| left > 0)
}

fn smp_visibility() -> bool {
    let initial_cpu = smp_get_processor_id();
    let Some(cpu) = smp_cpu_manager()
        .present_cpus()
        .iter_cpu()
        .find(|cpu| *cpu != initial_cpu && smp_cpu_manager().is_online_cpu(*cpu))
    else {
        return false;
    };
    let layout = Layout::from_size_align(MMArch::PAGE_SIZE, MMArch::PAGE_SIZE).unwrap();
    let Ok(mut allocation) = VmAllocation::try_new_zeroed(layout) else {
        return false;
    };
    allocation.as_mut_slice().fill(0x36);
    let address = allocation.address().data();
    // A pinned caller cannot migrate onto the reader's CPU while it is keeping
    // its old translation cached. Ordinary debugfs callers use the sleepable
    // handshake; dunitest pins its callers and exercises the stronger branch.
    let allowed = ProcessManager::current_pcb().sched_info().cpus_allowed();
    let strong = allowed.iter_cpu().count() == 1 && allowed.get(initial_cpu).unwrap_or(false);
    let state = Arc::new(Visibility {
        first_read: Completion::new(),
        read_replacement: Completion::new(),
        replacement_ready: AtomicBool::new(false),
        abort: AtomicBool::new(false),
        first_ok: AtomicBool::new(false),
        second_ok: AtomicBool::new(false),
    });
    let remote = state.clone();
    let closure = KernelThreadClosure::EmptyClosure((
        Box::new(move || {
            let preempt = strong.then(PreemptGuard::new);
            // Volatile loads make both checks actual remote translation accesses.
            let read = |pattern| {
                (0..MMArch::PAGE_SIZE).all(|offset| unsafe {
                    core::ptr::read_volatile((address + offset) as *const u8) == pattern
                })
            };
            remote.first_ok.store(
                smp_get_processor_id() == cpu && read(0x36),
                Ordering::Release,
            );
            remote.first_read.complete();
            let signalled = if strong {
                let started = clock();
                while !remote.replacement_ready.load(Ordering::Acquire)
                    && clock().wrapping_sub(started) < 5 * HZ
                {
                    core::hint::spin_loop();
                }
                remote.replacement_ready.load(Ordering::Acquire)
            } else {
                wait(&remote.read_replacement)
            };
            if !signalled || remote.abort.load(Ordering::Acquire) {
                return 1;
            }
            remote.second_ok.store(read(0xa9), Ordering::Release);
            drop(preempt);
            i32::from(!remote.second_ok.load(Ordering::Acquire))
        }),
        (),
    ));
    let Some(worker) =
        KernelThreadMechanism::create_on_cpu(closure, "vmalloc-tlb-test".to_string(), cpu)
    else {
        return false;
    };
    if ProcessManager::wakeup(&worker).is_err() {
        state.abort.store(true, Ordering::Release);
        state.replacement_ready.store(true, Ordering::Release);
        state.read_replacement.complete();
        let _ = KernelThreadMechanism::stop(&worker);
        return false;
    }
    if !wait(&state.first_read) {
        // Keep the old mapping alive until even a late first read has stopped.
        state.abort.store(true, Ordering::Release);
        state.replacement_ready.store(true, Ordering::Release);
        state.read_replacement.complete();
        let _ = KernelThreadMechanism::stop(&worker);
        return false;
    }
    // The remote has completed its old read and is blocked on the second gate.
    // Retain the old PFN separately so stale translations cannot accidentally
    // observe a replacement allocated from that same PFN.
    let old_frame = allocation.chunks.as_ref().unwrap().frames[0];
    drop(allocation);
    let mut retained_old = None;
    let replacement = VmAllocation::allocate_with(layout, &mut FailureBudget(None), &mut || {
        let frame = frame_zeroed()?;
        if frame == old_frame {
            retained_old = Some(frame);
            frame_zeroed()
        } else {
            Ok(frame)
        }
    });
    let reused = replacement
        .as_ref()
        .is_ok_and(|allocation| allocation.address().data() == address);
    let mut replacement = replacement.ok();
    if let Some(allocation) = &mut replacement {
        allocation.as_mut_slice().fill(0xa9);
    }
    if !reused {
        state.abort.store(true, Ordering::Release);
    }
    state.replacement_ready.store(true, Ordering::Release);
    state.read_replacement.complete();
    let stopped = matches!(KernelThreadMechanism::stop(&worker), Ok(0));
    // The replacement remains mapped until the worker has stopped all accesses.
    drop(replacement);
    if let Some(frame) = retained_old {
        unsafe { LockedFrameAllocator.free_one(frame) };
    }
    reused
        && stopped
        && state.first_ok.load(Ordering::Acquire)
        && state.second_ok.load(Ordering::Acquire)
}

fn completion_deadline() -> bool {
    let completed = Completion::new();
    completed.complete();
    if completed.wait_for_completion_timeout(0) != Ok(1)
        || completed.wait_for_completion_timeout(0) != Ok(0)
    {
        return false;
    }
    completed.complete();
    if completed.wait_for_completion() != Ok(i64::MAX) {
        return false;
    }

    // Exercise durations after their numeric value is already in the past as
    // an absolute boot timestamp. This is what exposed the original bug.
    while clock() <= 50 {
        crate::sched::sched_yield();
    }
    let empty = Completion::new();
    let started = clock();
    if empty.wait_for_completion_timeout(2) != Ok(0) || clock().wrapping_sub(started) < 2 {
        return false;
    }

    let current_cpu = smp_get_processor_id();
    let Some(cpu) = smp_cpu_manager()
        .present_cpus()
        .iter_cpu()
        .find(|cpu| *cpu != current_cpu && smp_cpu_manager().is_online_cpu(*cpu))
    else {
        return false;
    };
    let ready = Arc::new(Completion::new());
    let start = Arc::new(Completion::new());
    let done = Arc::new(Completion::new());
    let remote_ready = ready.clone();
    let remote_start = start.clone();
    let remote_done = done.clone();
    let closure = KernelThreadClosure::EmptyClosure((
        Box::new(move || {
            remote_ready.complete();
            if remote_start.wait_for_completion().is_err() {
                return 1;
            }
            let started = clock();
            // Use the clock directly, independent of the Completion timer being
            // tested. Yielding keeps this delay safe even if the caller migrates.
            while clock().wrapping_sub(started) < 2 {
                crate::sched::sched_yield();
            }
            remote_done.complete();
            0
        }),
        (),
    ));
    let Some(worker) =
        KernelThreadMechanism::create_on_cpu(closure, "completion-deadline-test".to_string(), cpu)
    else {
        return false;
    };
    let woke = ProcessManager::wakeup(&worker).is_ok();
    let initialized = woke && ready.wait_for_completion().is_ok();
    let started = clock();
    start.complete();
    let observed = initialized
        && done
            .wait_for_completion_timeout(50)
            .is_ok_and(|left| left > 0)
        && clock().wrapping_sub(started) >= 2;
    // Even a premature timeout leaves the worker running for its two ticks;
    // join it before releasing any test state or returning a failed report.
    let joined = matches!(KernelThreadMechanism::stop(&worker), Ok(0));
    observed && joined
}

pub(crate) fn report() -> String {
    let _serial = TEST_LOCK.lock();
    let cases = [
        ("zero_alignment_cross_page", basic()),
        ("partial_failure_rollback", rollback()),
        ("noncontiguous_backing", noncontiguous()),
        ("shared_table_lifetime", shared_table()),
        ("layout_boundaries", boundaries()),
        ("accounting", accounting()),
        ("smp_visibility_reuse", smp_visibility()),
        ("completion_deadline", completion_deadline()),
    ];
    let failures = cases.iter().filter(|(_, ok)| !ok).count();
    let mut report = String::new();
    writeln!(
        report,
        "status={}",
        if failures == 0 { "ok" } else { "fail" }
    )
    .unwrap();
    for (name, ok) in cases {
        writeln!(report, "{name}={}", if ok { "ok" } else { "fail" }).unwrap();
    }
    writeln!(report, "summary_fail={failures}").unwrap();
    report
}
