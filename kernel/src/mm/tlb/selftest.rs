//! Transaction-local teardown evidence. No remapping/publication flush is used.
//!
//! The one registration slot only connects a private fixture to real munmap's
//! gather; it is not an observer of ordinary mm traffic. The caller owns every
//! observer before revoking PTEs, so skipping an entire flush is detectable.
use super::*;
use crate::{
    libs::mutex::Mutex,
    mm::{
        allocator::page_frame::PageFrameCount,
        page::page_manager_lock,
        syscall::{MapFlags, ProtFlags},
        vmalloc::VmAllocation,
        PageTableKind,
    },
    process::{
        kthread::{KernelThreadClosure, KernelThreadMechanism},
        preempt::PreemptGuard,
        ProcessManager,
    },
    sched::completion::Completion,
    time::clocksource::HZ,
};
use alloc::{
    boxed::Box,
    format,
    string::{String, ToString},
};
use core::{
    alloc::Layout,
    sync::atomic::{AtomicUsize, Ordering},
};

static TEST_LOCK: Mutex<()> = Mutex::new(());
static USER_FIXTURE: SpinLock<Option<Arc<Observer>>> = SpinLock::new(None);
// Bounded even when both CPUs have IRQs disabled and the timer cannot advance.
const SPINS: usize = 20_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Normal,
    SkipFlush,
    SkipAck,
}

#[derive(Debug)]
pub(crate) struct Observer {
    pub(crate) start: VirtAddr,
    pub(crate) end: VirtAddr,
    mm_id: usize,
    target: ProcessorId,
    root: usize,
    mode: Mode,
    pub(crate) frame: AtomicUsize,
    cleared: AtomicBool,
    gate: AtomicBool,
    pending: AtomicBool,
    remote: AtomicBool,
    returned: AtomicBool,
    negative: AtomicBool,
    failed: AtomicBool,
    frees: AtomicUsize,
    tables: AtomicUsize,
}

impl Observer {
    fn new(
        start: VirtAddr,
        mm: Option<&Arc<AddressSpace>>,
        target: ProcessorId,
        mode: Mode,
    ) -> Arc<Self> {
        Arc::new(Self {
            start,
            end: start + MMArch::PAGE_SIZE,
            mm_id: mm.map_or(0, |m| Arc::as_ptr(m) as usize),
            target,
            root: mm.map_or(0, |m| m.table_paddr().data()),
            mode,
            frame: AtomicUsize::new(0),
            cleared: AtomicBool::new(false),
            gate: AtomicBool::new(false),
            pending: AtomicBool::new(false),
            remote: AtomicBool::new(false),
            returned: AtomicBool::new(false),
            negative: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            frees: AtomicUsize::new(0),
            tables: AtomicUsize::new(0),
        })
    }
    pub(crate) fn cleared(&self, absent: bool) {
        if !absent {
            self.failed.store(true, Ordering::Release);
        }
        self.cleared.store(absent, Ordering::Release);
    }
    pub(crate) fn skip_flush(&self) -> bool {
        self.mode == Mode::SkipFlush
    }
    pub(crate) fn skip_ack(&self) -> bool {
        self.mode == Mode::SkipAck
    }
    pub(crate) fn wait_pending(&self, cpu: ProcessorId) {
        if cpu == self.target {
            self.pending.store(true, Ordering::Release);
            self.gate.store(true, Ordering::Release);
        }
    }
    pub(crate) fn before_remote(&self, info: &FlushTlbInfo) {
        if smp_get_processor_id() != self.target {
            return;
        }
        let identity = info.mm.as_ref().map_or(0, |mm| Arc::as_ptr(mm) as usize);
        let loaded = info.mm.as_ref().is_none_or(|mm| {
            tlb_state_force(self.target).loaded_is(mm)
                && unsafe { MMArch::table(PageTableKind::User) }.data() == self.root
        });
        if identity != self.mm_id
            || !loaded
            || info.start > self.start
            || (!info.is_flush_all() && info.end < self.end)
            || !self.cleared.load(Ordering::Acquire)
        {
            self.failed.store(true, Ordering::Release);
        }
        for _ in 0..SPINS {
            if self.gate.load(Ordering::Acquire) {
                return;
            }
            spin_loop();
        }
        self.failed.store(true, Ordering::Release);
        // Fail the observation, but still execute the real invalidation/ack.
    }
    pub(crate) fn after_remote(&self) {
        if smp_get_processor_id() == self.target {
            self.remote.store(true, Ordering::Release);
        }
    }
    pub(crate) fn missing_ack(&self) {
        // Called before FlushTlbInfo's stack frame can be left. This is an
        // expected violation, not a completed successful shootdown.
        if self.remote.load(Ordering::Acquire) || self.gate.load(Ordering::Acquire) {
            self.failed.store(true, Ordering::Release);
        } else {
            self.negative.store(true, Ordering::Release);
        }
        self.gate.store(true, Ordering::Release);
    }
    pub(crate) fn flush_returned(&self) {
        if !self.remote.load(Ordering::Acquire) {
            self.failed.store(true, Ordering::Release);
        }
        self.returned.store(true, Ordering::Release);
    }
    /// Nonblocking check immediately before the actual retirement operation.
    pub(crate) fn before_free(&self) -> bool {
        let safe = self.cleared.load(Ordering::Acquire)
            && self.remote.load(Ordering::Acquire)
            && self.returned.load(Ordering::Acquire);
        if !safe {
            if self.mode == Mode::SkipFlush {
                self.negative.store(true, Ordering::Release);
            } else {
                self.failed.store(true, Ordering::Release);
            }
        }
        safe
    }
    pub(crate) fn frame_freed(&self) {
        self.frees.fetch_add(1, Ordering::Release);
    }
    pub(crate) fn tables_freed(&self) {
        self.tables.fetch_add(1, Ordering::Release);
    }
    pub(crate) fn fail(&self) {
        self.failed.store(true, Ordering::Release);
    }
    fn passed(&self) -> bool {
        !self.failed.load(Ordering::Acquire)
            && self.cleared.load(Ordering::Acquire)
            && self.remote.load(Ordering::Acquire)
            && self.returned.load(Ordering::Acquire)
            && self.frees.load(Ordering::Acquire) == 1
            && self.tables.load(Ordering::Acquire) > 0
            && (self.mode == Mode::SkipFlush || self.mode == Mode::SkipAck)
                == self.negative.load(Ordering::Acquire)
            && (self.mode == Mode::SkipAck || self.pending.load(Ordering::Acquire))
    }
}

pub(crate) fn observer_for(mm: &Arc<AddressSpace>) -> Option<Arc<Observer>> {
    USER_FIXTURE
        .lock()
        .as_ref()
        .filter(|o| o.mm_id == Arc::as_ptr(mm) as usize)
        .cloned()
}

fn wait(c: &Completion) -> bool {
    c.wait_for_completion_timeout((5 * HZ) as i64)
        .is_ok_and(|n| n > 0)
}

/// Only this fixture borrows a hardware mm: never scheduled while borrowed,
/// with actual root/active/loaded tracking restored before preemption resumes.
struct BorrowDiagnostic {
    stage: AtomicUsize,
    saved_is_idle: AtomicBool,
    saved_active: AtomicBool,
}

struct BorrowedMm {
    saved: Arc<AddressSpace>,
    private: Arc<AddressSpace>,
    saved_active: bool,
}
impl BorrowedMm {
    fn enter(private: &Arc<AddressSpace>, diagnostic: &BorrowDiagnostic) -> Option<Self> {
        let stage = &diagnostic.stage;
        stage.store(1, Ordering::Release);
        let irq = unsafe { CurrentIrqArch::save_and_disable_irq() };
        let saved = tlb_state_loaded_mm()?;
        let hardware_root = unsafe { MMArch::table(PageTableKind::User) };
        stage.store(21, Ordering::Release);
        let cpu = smp_get_processor_id();
        if hardware_root != saved.table_paddr() {
            return None;
        }
        let saved_active = saved.active_cpus.lock().get(cpu).unwrap_or(false);
        let saved_is_idle = Arc::ptr_eq(&saved, &crate::mm::IDLE_PROCESS_ADDRESS_SPACE());
        diagnostic
            .saved_active
            .store(saved_active, Ordering::Release);
        diagnostic
            .saved_is_idle
            .store(saved_is_idle, Ordering::Release);
        stage.store(22, Ordering::Release);
        // Like Linux init_mm, the immortal bootstrap idle root is excluded
        // from user-mm target tracking (bootstrap retirement uses AllOnline).
        // An ordinary user mm must still have its real holder bit published.
        if !saved_active && !saved_is_idle {
            return None;
        }
        stage.store(3, Ordering::Release);
        private.active_cpus_set(cpu);
        unsafe {
            private.make_current();
        }
        if saved_active {
            saved.active_cpus_clear(cpu);
        }
        unsafe {
            tlb_state_set_loaded_mm(private.clone());
        }
        let borrowed = Self {
            saved,
            private: private.clone(),
            saved_active,
        };
        if unsafe { MMArch::table(PageTableKind::User) } != private.table_paddr()
            || !tlb_state_force(cpu).loaded_is(private)
            || !private.active_cpus.lock().get(cpu).unwrap_or(false)
        {
            drop(borrowed);
            drop(irq);
            return None;
        }
        stage.store(4, Ordering::Release);
        drop(irq);
        Some(borrowed)
    }
}
impl Drop for BorrowedMm {
    fn drop(&mut self) {
        let irq = unsafe { CurrentIrqArch::save_and_disable_irq() };
        let cpu = smp_get_processor_id();
        // Restore the incoming init_mm bookkeeping exactly; do not invent
        // an active bit merely to satisfy the fixture's admission checks.
        if self.saved_active {
            self.saved.active_cpus_set(cpu);
        }
        unsafe {
            self.saved.make_current();
        }
        self.private.active_cpus_clear(cpu);
        unsafe {
            tlb_state_set_loaded_mm(self.saved.clone());
        }
        assert_eq!(
            unsafe { MMArch::table(PageTableKind::User) },
            self.saved.table_paddr()
        );
        assert_eq!(
            self.saved.active_cpus.lock().get(cpu).unwrap_or(false),
            self.saved_active
        );
        assert!(tlb_state_force(cpu).loaded_is(&self.saved));
        assert!(!self.private.active_cpus.lock().get(cpu).unwrap_or(false));
        drop(irq);
    }
}

fn run(user: bool, mode: Mode, cpu: ProcessorId, diagnostic: &mut String) -> bool {
    use core::fmt::Write;
    macro_rules! fail {
        ($stage:literal, $error:expr) => {{
            let _ = writeln!(diagnostic, "stage={} detail={:?}", $stage, $error);
            return false;
        }};
    }
    let current = ProcessManager::current_pcb();
    let private = if user {
        match AddressSpace::new(false, current.cred().user_ns.clone(), 0) {
            Ok(mm) => Some(mm),
            Err(error) => fail!("mm_new", error),
        }
    } else {
        None
    };
    let mut allocation = None;
    let start = if let Some(mm) = &private {
        match mm.map_anonymous_wait(
            VirtAddr::new(0x4000_0000),
            MMArch::PAGE_SIZE,
            ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
            MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS,
            true,
            true,
        ) {
            Ok(page) => page.virt_address(),
            Err(error) => fail!("map_anonymous", error),
        }
    } else {
        let layout = Layout::from_size_align(MMArch::PAGE_SIZE, MMArch::PAGE_SIZE).unwrap();
        match VmAllocation::try_new_zeroed(layout) {
            Ok(a) => {
                let start = a.address();
                allocation = Some(a);
                start
            }
            Err(error) => fail!("vmalloc", error),
        }
    };
    let observer = Observer::new(start, private.as_ref(), cpu, mode);
    if let Some(mm) = &private {
        let guard = mm.read();
        let Some((phys, _)) = guard.user_mapper.utable.translate(start) else {
            fail!("pte_not_present", start);
        };
        let Some(page) = page_manager_lock().get_registered(&phys) else {
            fail!("page_unregistered", phys);
        };
        if !page.read().individually_owns_frame() {
            fail!("frame_not_independent", phys);
        }
        observer.frame.store(phys.data(), Ordering::Release);
        drop(page);
        drop(guard);
    }
    let ready = Arc::new(Completion::new());
    let done = Arc::new(AtomicBool::new(false));
    let read_ok = Arc::new(AtomicBool::new(false));
    // One bounded diagnostic per fixture, never an IPI/event log:
    // 1=no saved mm, 21=saved root mismatch, 22=saved active bit missing,
    // 3=private identity rejected, 4=borrow established, 0=not entered.
    let borrow_stage = Arc::new(BorrowDiagnostic {
        stage: AtomicUsize::new(0),
        saved_is_idle: AtomicBool::new(false),
        saved_active: AtomicBool::new(false),
    });
    let worker_borrow_stage = borrow_stage.clone();
    let worker_ready = ready.clone();
    let worker_done = done.clone();
    let worker_ok = read_ok.clone();
    let worker_mm = private.clone();
    let closure = KernelThreadClosure::EmptyClosure((
        Box::new(move || {
            let preempt = PreemptGuard::new();
            let borrowed = match worker_mm.as_ref() {
                Some(mm) => match BorrowedMm::enter(mm, &worker_borrow_stage) {
                    Some(mm) => Some(mm),
                    None => {
                        worker_ready.complete();
                        return 1;
                    }
                },
                None => None,
            };
            // Eagerly mapped before entering the non-sleepable borrow window.
            let value = unsafe { core::ptr::read_volatile(start.as_ptr::<u8>()) };
            worker_ok.store(
                smp_get_processor_id() == cpu && value == 0,
                Ordering::Release,
            );
            worker_ready.complete();
            let mut completed = false;
            // Outer worker wait is IRQ-enabled; use a deadline rather than
            // schedule away and accidentally erase the old translation.
            let began = crate::time::timer::clock();
            while crate::time::timer::clock().wrapping_sub(began) < 5 * HZ {
                if worker_done.load(Ordering::Acquire) {
                    completed = true;
                    break;
                }
                spin_loop();
            }
            drop(borrowed);
            drop(preempt);
            i32::from(!completed)
        }),
        (),
    ));
    let Some(worker) =
        KernelThreadMechanism::create_on_cpu(closure, "tlb-teardown".to_string(), cpu)
    else {
        return false;
    };
    if ProcessManager::wakeup(&worker).is_err() {
        done.store(true, Ordering::Release);
        let _ = KernelThreadMechanism::stop(&worker);
        return false;
    }
    let ready_ok = wait(&ready) && read_ok.load(Ordering::Acquire);
    if ready_ok {
        if let Some(mm) = &private {
            *USER_FIXTURE.lock() = Some(observer.clone());
            let result = mm.munmap_wait(
                crate::mm::allocator::page_frame::VirtPageFrame::new(start),
                PageFrameCount::new(1),
            );
            *USER_FIXTURE.lock() = None;
            if result.is_err() {
                observer.fail();
            }
        } else {
            allocation
                .as_mut()
                .unwrap()
                .set_teardown_observer(observer.clone());
            drop(allocation.take());
        }
    }
    done.store(true, Ordering::Release);
    let stopped = matches!(KernelThreadMechanism::stop(&worker), Ok(0));
    let passed = ready_ok && stopped && observer.passed();
    if !passed {
        let _ = writeln!(diagnostic,
            "stage=teardown ready={} stopped={} borrow={} saved_is_idle={} saved_active={} cleared={} pending={} remote={} returned={} negative={} failed={} frees={} tables={}",
            ready_ok, stopped, borrow_stage.stage.load(Ordering::Acquire),
            borrow_stage.saved_is_idle.load(Ordering::Acquire), borrow_stage.saved_active.load(Ordering::Acquire), observer.cleared.load(Ordering::Acquire),
            observer.pending.load(Ordering::Acquire), observer.remote.load(Ordering::Acquire),
            observer.returned.load(Ordering::Acquire), observer.negative.load(Ordering::Acquire),
            observer.failed.load(Ordering::Acquire), observer.frees.load(Ordering::Acquire),
            observer.tables.load(Ordering::Acquire));
    }
    if passed && user {
        let _ = writeln!(
            diagnostic,
            "saved_is_idle={} saved_active={}",
            borrow_stage.saved_is_idle.load(Ordering::Acquire),
            borrow_stage.saved_active.load(Ordering::Acquire)
        );
    }
    passed
}

pub(super) fn report() -> String {
    let _serial = TEST_LOCK.lock();
    let _vmalloc_serial = crate::mm::vmalloc::lock_teardown_fixture();
    let here = smp_get_processor_id();
    let target = smp_cpu_manager()
        .present_cpus()
        .iter_cpu()
        .find(|cpu| *cpu != here && smp_cpu_manager().is_online_cpu(*cpu));
    let mut report = String::new();
    use core::fmt::Write;
    let _ = writeln!(
        report,
        "smp_available={}",
        if target.is_some() { "ok" } else { "fail" }
    );
    let mut failures = usize::from(target.is_none());
    for (user, mode, key) in [
        (false, Mode::Normal, "kernel_teardown_order"),
        (false, Mode::SkipFlush, "kernel_missing_flush_detected"),
        (false, Mode::SkipAck, "kernel_missing_ack_detected"),
        (true, Mode::Normal, "user_teardown_order"),
        (true, Mode::SkipFlush, "user_missing_flush_detected"),
        (true, Mode::SkipAck, "user_missing_ack_detected"),
    ] {
        let mut diagnostic = String::new();
        let ok = target.is_some_and(|cpu| run(user, mode, cpu, &mut diagnostic));
        if !diagnostic.is_empty() {
            let _ = write!(report, "{key}_diagnostic={diagnostic}");
        }
        failures += usize::from(!ok);
        let _ = writeln!(report, "{key}={}", if ok { "ok" } else { "fail" });
    }
    format!(
        "status={}\n{}summary_fail={}\n",
        if failures == 0 { "ok" } else { "fail" },
        report,
        failures
    )
}
