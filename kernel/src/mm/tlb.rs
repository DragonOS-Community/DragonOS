//! mm-aware TLB shootdown infrastructure (Linux 6.6 style)
//!
//! This module unifies DragonOS TLB flushing around:
//! - `mm_cpumask` (`AddressSpace::active_cpus`): the set of CPUs that may currently hold TLB entries for this mm.
//! - `tlb_gen` (`AddressSpace::tlb_gen`): monotonically increasing generation counter for page table modifications.
//! - per-CPU `TlbState`: records the mm currently loaded on this CPU and the latest tlb_gen it has caught up to.
//! - `FlushTlbInfo`: cross-CPU shootdown context.
//! - `flush_tlb_multi`: synchronous cross-CPU broadcast with ack, using `CsdFlushTlb` for polling.
//!
//! Design invariants:
//! - INV-1: At any point, if CPU `c`'s hardware page table equals `mm.table_paddr`, then bit `c` in `mm.active_cpus` is 1.
//! - INV-2: `flush_tlb_*` must publish page table writes first, then `inc_mm_tlb_gen`, then read `active_cpus`.
//! - INV-3: Freeing physical pages / page-table pages must happen after shootdown completion (guaranteed by `MmuGather`).
//! - INV-4: When `flush_tlb_multi` returns, all target CPUs have finished executing `local_flush_tlb_func`.
//! - INV-5: When `freed_tables == true`, the receiving end must handle it (simplified: in this iteration the sender
//!   sends to all active CPUs regardless, and the receiver unconditionally executes).

use core::sync::atomic::{compiler_fence, AtomicUsize, Ordering};

#[cfg(target_arch = "x86_64")]
use core::{hint::spin_loop, sync::atomic::AtomicBool};

#[cfg(all(feature = "mm_selftest", target_arch = "x86_64"))]
pub(crate) mod selftest;
#[cfg(all(feature = "mm_selftest", target_arch = "x86_64"))]
pub(crate) fn teardown_selftest_report() -> alloc::string::String {
    selftest::report()
}

use alloc::sync::Arc;
#[allow(unused_imports)]
use alloc::vec::Vec;

use crate::{
    arch::{interrupt::ipi::send_ipi, CurrentIrqArch, MMArch},
    exception::{
        ipi::{IpiKind, IpiTarget},
        InterruptArch,
    },
    libs::cpumask::AtomicCpuMask,
    mm::{percpu::PerCpuVar, ucontext::AddressSpace, MemoryManagementArch, VirtAddr},
    smp::{
        core::smp_get_processor_id,
        cpu::{smp_cpu_manager, ProcessorId},
    },
};

#[cfg(target_arch = "x86_64")]
use crate::libs::spinlock::SpinLock;

/// Sentinel end value meaning "flush the entire mm"; combined with start=0 it means "do not use range-based flush".
pub const TLB_FLUSH_ALL: VirtAddr = VirtAddr::new(usize::MAX);

/// Cf. Linux `tlb_single_page_flush_ceiling`: above this threshold, fall back to full-mm flush.
pub const TLB_SINGLE_PAGE_FLUSH_CEILING: usize = 33;

/// Number of `spin_loop()` iterations after which a still-unacknowledged
/// shootdown target is reported once. A live target acknowledges within a few
/// microseconds; reaching this bound means the target is wedged, which is a bug
/// worth surfacing even though the initiator must keep waiting for correctness.
const CSD_STALL_WARN_SPINS: u64 = 50_000_000;

/// Entry-point identifiers reported by [`TLB_DIAG`].
pub const SHOOTDOWN_ENTRY_MM_RANGE: usize = 0;
pub const SHOOTDOWN_ENTRY_KERNEL_RANGE: usize = 1;

/// Context for a single shootdown. Lives on the initiator's stack, spanning the entire IPI wait window.
#[derive(Debug)]
#[allow(dead_code)]
pub struct FlushTlbInfo {
    /// Target address space
    pub mm: Option<Arc<AddressSpace>>,
    /// Start virtual address (inclusive, aligned to stride)
    pub start: VirtAddr,
    /// End virtual address (exclusive). If `TLB_FLUSH_ALL`, invalidate the entire mm.
    pub end: VirtAddr,
    /// New generation number after increment
    pub new_tlb_gen: u64,
    /// Page size shift (currently fixed at `PAGE_SHIFT`; huge-page stride not yet supported)
    pub stride_shift: u8,
    /// Whether page-table pages were also freed.
    /// Current simplified policy: the sender sends IPI to all CPUs in `active_cpus`
    /// regardless of the `freed_tables` value, and the receiver always performs the flush.
    /// This field is retained for future lazy TLB decision-making.
    pub freed_tables: bool,
    /// Initiating CPU
    pub initiating_cpu: ProcessorId,
    #[cfg(all(feature = "mm_selftest", target_arch = "x86_64"))]
    pub(crate) observer: Option<Arc<selftest::Observer>>,
}

impl FlushTlbInfo {
    /// Check whether this is a full-mm flush
    #[inline]
    pub fn is_flush_all(&self) -> bool {
        self.end == TLB_FLUSH_ALL
    }

    /// Number of pages in the flush range (in stride units), used to decide whether to degrade to full-mm flush
    #[inline]
    pub fn range_pages(&self) -> usize {
        if self.is_flush_all() {
            usize::MAX
        } else {
            let stride = 1usize << self.stride_shift;
            let len = self.end.data().saturating_sub(self.start.data());
            len.div_ceil(stride)
        }
    }
}

/// Per-CPU TLB state
#[derive(Debug)]
pub struct TlbState {
    /// The mm currently loaded in hardware on this CPU (weak reference; drop does not prevent mm release)
    loaded_mm: Option<Arc<AddressSpace>>,
    /// The mm tlb_gen this CPU has caught up to
    loaded_tlb_gen: u64,
}

impl TlbState {
    const fn new() -> Self {
        Self {
            loaded_mm: None,
            loaded_tlb_gen: 0,
        }
    }

    /// Get the currently loaded mm (clones the Arc)
    #[allow(dead_code)]
    pub fn loaded_mm(&self) -> Option<Arc<AddressSpace>> {
        self.loaded_mm.clone()
    }

    /// Check whether this CPU currently has the given mm loaded.
    ///
    /// Compares Arc pointer equality.
    pub fn loaded_is(&self, mm: &Arc<AddressSpace>) -> bool {
        match &self.loaded_mm {
            Some(cur) => Arc::ptr_eq(cur, mm),
            None => false,
        }
    }

    #[allow(dead_code)]
    pub fn loaded_tlb_gen(&self) -> u64 {
        self.loaded_tlb_gen
    }
}

/// Return this CPU's hardware-loaded mm tracked by the lazy TLB state.
///
/// Context switch uses this when the previous task has already dropped its
/// user-visible mm but the CPU is still running on an active mm.
pub fn tlb_state_loaded_mm() -> Option<Arc<AddressSpace>> {
    tlb_state_local_mut().loaded_mm()
}

/// Per-CPU CSD (Call Single Data) for synchronous ack during TLB shootdown.
///
/// Currently only serves FlushTLB; not abstracted into a generic `smp_call_function`.
///
/// x86_64 only; RISC-V's `send_ipi(FlushTLB, ..)` uses synchronous SBI `remote_sfence_vma`
/// and does not need CSD.
#[cfg(target_arch = "x86_64")]
pub struct CsdFlushTlb {
    /// Pointer to `FlushTlbInfo` written by the initiator; read by the receiver.
    info: SpinLock<Option<*const FlushTlbInfo>>,
    /// Set to true by the target CPU upon completion.
    done: AtomicBool,
}

// SAFETY: The pointer itself is unsafe, but it points to an object on the initiator's stack,
// synchronized via the done flag.
#[cfg(target_arch = "x86_64")]
unsafe impl Sync for CsdFlushTlb {}
#[cfg(target_arch = "x86_64")]
unsafe impl Send for CsdFlushTlb {}

#[cfg(target_arch = "x86_64")]
impl CsdFlushTlb {
    const fn new() -> Self {
        Self {
            info: SpinLock::new(None),
            done: AtomicBool::new(true),
        }
    }
}

/// Per-CPU TLB state (currently loaded mm and latest generation per CPU)
static mut TLB_STATE: Option<PerCpuVar<TlbState>> = None;

/// Per-CPU CSD slot; each CPU allows at most one in-flight TLB shootdown at a time
#[cfg(target_arch = "x86_64")]
static mut CSD_FLUSH_TLB: Option<PerCpuVar<CsdFlushTlb>> = None;

/// Serialize cross-core shootdown.
///
/// Currently uses a single global lock to ensure initiators don't share CSD slots;
/// can be upgraded to per-initiator CSD pools in the future.
#[cfg(target_arch = "x86_64")]
static FLUSH_TLB_GLOBAL_LOCK: SpinLock<()> = SpinLock::new(());

/// Initialize this module. Must be called after `PerCpu::init()`.
pub fn tlb_init() {
    let cpu_num = crate::mm::percpu::PerCpu::MAX_CPU_NUM as usize;

    let mut states: Vec<TlbState> = Vec::with_capacity(cpu_num);
    for _ in 0..cpu_num {
        states.push(TlbState::new());
    }
    unsafe {
        TLB_STATE = Some(PerCpuVar::new(states).expect("PerCpuVar length mismatch"));
    }

    #[cfg(target_arch = "x86_64")]
    {
        let mut csds: Vec<CsdFlushTlb> = Vec::with_capacity(cpu_num);
        for _ in 0..cpu_num {
            csds.push(CsdFlushTlb::new());
        }
        unsafe {
            CSD_FLUSH_TLB = Some(PerCpuVar::new(csds).expect("PerCpuVar length mismatch"));
        }
    }
}

#[inline]
fn tlb_state() -> &'static PerCpuVar<TlbState> {
    // Must not be called before initialization (tlb_init runs before idle first enters)
    unsafe { TLB_STATE.as_ref().expect("tlb_state not initialized") }
}

#[cfg(target_arch = "x86_64")]
#[inline]
fn csd_flush_tlb() -> &'static PerCpuVar<CsdFlushTlb> {
    unsafe {
        CSD_FLUSH_TLB
            .as_ref()
            .expect("csd_flush_tlb not initialized")
    }
}

/// Get the TlbState (mutable) for a specified CPU. Only use when interrupts are disabled or other synchronization guarantees exist.
#[allow(dead_code)]
#[inline]
pub fn tlb_state_force_mut(cpu: ProcessorId) -> &'static mut TlbState {
    unsafe { tlb_state().force_get_mut(cpu) }
}

/// Get the TlbState (mutable) for the current CPU. The caller must ensure preemption/interrupts won't cause concurrent access from another CPU.
#[inline]
pub fn tlb_state_local_mut() -> &'static mut TlbState {
    tlb_state().get_mut()
}

/// Get the TlbState (read-only) for a specified CPU.
#[inline]
pub fn tlb_state_force(cpu: ProcessorId) -> &'static TlbState {
    unsafe { tlb_state().force_get(cpu) }
}

/// After context switch, update this CPU's TlbState to the new mm.
///
/// # Safety
///
/// - The caller must execute this with interrupts disabled (to ensure per-CPU data is not preempted).
/// - `mm.active_cpus` must already include this CPU (set the bit before calling this function during switch).
pub unsafe fn tlb_state_set_loaded_mm(mm: Arc<AddressSpace>) {
    let st = tlb_state_local_mut();
    let new_gen = mm.tlb_gen.load(Ordering::SeqCst);
    st.loaded_mm = Some(mm);
    st.loaded_tlb_gen = new_gen;
}

/// Clear this CPU's loaded_mm (used for the final state when a process exits and user_vm is set to None).
///
/// # Safety
///
/// - The caller must execute this with interrupts disabled.
#[allow(dead_code)]
pub unsafe fn tlb_state_clear_loaded_mm() {
    let st = tlb_state_local_mut();
    st.loaded_mm = None;
    st.loaded_tlb_gen = 0;
}

/// Context-aware local TLB flush.
///
/// Policy:
/// - Full mm or range exceeding `TLB_SINGLE_PAGE_FLUSH_CEILING` pages → `invalidate_all`.
/// - Otherwise, invalidate page by page via `invalidate_page`.
///
/// Only updates `loaded_tlb_gen` when this CPU's loaded_mm matches `info.mm`.
pub fn local_flush_tlb_func(info: &FlushTlbInfo) {
    let loaded_matches = info.mm.as_ref().is_none_or(|mm| {
        let st = tlb_state_force(smp_get_processor_id());
        st.loaded_is(mm)
    });

    if !loaded_matches {
        // This CPU has already switched to a different mm; the previous CR3 write implicitly
        // invalidated the entire TLB, no further flush needed.
        return;
    }

    // When intermediate page-table pages have been freed, per-page invlpg cannot clear
    // Paging-Structure Cache (PSC) entries pointing to the reclaimed intermediate PT;
    // a full-mm invalidation is required, matching Linux `tlb->freed_tables` semantics.
    let must_full = info.is_flush_all()
        || info.freed_tables
        || info.range_pages() > TLB_SINGLE_PAGE_FLUSH_CEILING;

    if must_full {
        unsafe { MMArch::invalidate_all() };
    } else {
        let stride = 1usize << info.stride_shift;
        let mut addr = info.start.data();
        let end = info.end.data();
        while addr < end {
            unsafe { MMArch::invalidate_page(VirtAddr::new(addr)) };
            addr = addr.saturating_add(stride);
        }
    }

    if info.mm.is_some() {
        let st = tlb_state_local_mut();
        if st.loaded_tlb_gen < info.new_tlb_gen {
            st.loaded_tlb_gen = info.new_tlb_gen;
        }
    }
}

/// Remote IPI handler entry point.
///
/// Called by `FlushTLBIpiHandler::handle`; reads `&FlushTlbInfo` from the per-CPU CSD,
/// executes `local_flush_tlb_func`, and atomically sets `done`.
///
/// x86_64 only; RISC-V's `send_ipi(FlushTLB, ..)` uses SBI `remote_sfence_vma` and does not go through here.
pub fn remote_flush_tlb_on_ipi() {
    #[cfg(target_arch = "x86_64")]
    {
        let cpu = smp_get_processor_id();
        let csd = unsafe { csd_flush_tlb().force_get(cpu) };
        let info_ptr: *const FlushTlbInfo = {
            let guard = csd.info.lock();
            match *guard {
                Some(p) => p,
                None => {
                    // Should not happen; defensively set done to avoid initiator deadlock
                    csd.done.store(true, Ordering::Release);
                    return;
                }
            }
        };

        // SAFETY: the initiator's stack frame remains valid throughout the polling window in `flush_tlb_multi`;
        // it will not be freed before done=true.
        let info = unsafe { &*info_ptr };

        #[cfg(feature = "mm_selftest")]
        if let Some(observer) = &info.observer {
            observer.before_remote(info);
        }
        local_flush_tlb_func(info);
        #[cfg(feature = "mm_selftest")]
        if let Some(observer) = &info.observer {
            observer.after_remote();
        }

        compiler_fence(Ordering::SeqCst);
        csd.done.store(true, Ordering::Release);
    }

    #[cfg(not(target_arch = "x86_64"))]
    {
        // Other architectures should not reach this function via IPI handler;
        // if called by mistake, perform a conservative full-mm invalidation as a fallback.
        unsafe { MMArch::invalidate_all() };
    }
}

/// Issue a synchronous TLB shootdown to a set of target CPUs.
///
/// Initiator requirements:
/// 1. Page table writes are complete and `inc_mm_tlb_gen` has been called.
/// 2. `target_cpus` must not include this CPU (local flush is done by the caller after sending IPIs).
///
/// Upon return, all CPUs in `target_cpus` have completed this shootdown (INV-4).
fn flush_tlb_multi(target_cpus: &AtomicCpuMask, info: &FlushTlbInfo) {
    // Filter out offline CPUs to avoid sending IPIs to not-yet-started / offline APICs.
    // Capture online eligibility once. Every CSD preparation, IPI send and
    // acknowledgement below uses this same private, immutable stack mask.
    let active = AtomicCpuMask::new();
    let my_cpu = smp_get_processor_id();
    for cpu in smp_cpu_manager().present_cpus().iter_cpu() {
        if cpu != my_cpu && target_cpus.get(cpu) && smp_cpu_manager().is_online_cpu(cpu) {
            active.set(cpu);
        }
    }

    #[cfg(target_arch = "x86_64")]
    {
        // Prepare CSD slots for each target CPU
        for cpu in active.iter_cpu() {
            if cpu == my_cpu {
                continue;
            }
            let csd = unsafe { csd_flush_tlb().force_get(cpu) };
            {
                let mut g = csd.info.lock();
                *g = Some(info as *const FlushTlbInfo);
            }
            csd.done.store(false, Ordering::SeqCst);
        }

        compiler_fence(Ordering::SeqCst);

        // Send IPIs (one Specified per CPU, avoiding "Other" broadcast that would cause unrelated CPUs to take extra paths)
        for cpu in active.iter_cpu() {
            if cpu == my_cpu {
                continue;
            }
            send_ipi(IpiKind::FlushTLB, IpiTarget::Specified(cpu));
        }

        #[cfg(feature = "mm_selftest")]
        if let Some(observer) = &info.observer {
            if observer.skip_ack() {
                // Detect the prospective premature return while every CSD
                // still points into this live stack frame; then rescue safely.
                observer.missing_ack();
            }
        }

        // Poll for done.
        //
        // Liveness: a target CPU that has left `Online` (panic/halt or hot-unplug)
        // will never execute `remote_flush_tlb_on_ipi` again, so it can never
        // acknowledge. It also executes no further memory accesses, which means the
        // shootdown it was asked to perform is moot — continuing to wait would pin
        // the initiator (preemption is disabled and the caller may hold locks)
        // forever and turn a local CPU failure into a machine-wide hang.
        // So: stop waiting as soon as the target is observed to be non-online.
        for cpu in active.iter_cpu() {
            if cpu == my_cpu {
                continue;
            }
            let csd = unsafe { csd_flush_tlb().force_get(cpu) };
            #[cfg(feature = "mm_selftest")]
            let mut test_spins = 0usize;
            let mut spins = 0u64;
            while !csd.done.load(Ordering::Acquire) {
                spins += 1;
                // Checking the (cross-CPU) hotplug state on every iteration would
                // add pointless global traffic to the normal microsecond-scale
                // acknowledgement window; a target that goes away stays away.
                if spins & 1023 == 0 && !smp_cpu_manager().is_online_cpu(cpu) {
                    break;
                }
                if spins == CSD_STALL_WARN_SPINS {
                    // Should be unreachable: a live target acks within
                    // microseconds. Do not abort (INV-3 needs the ack); only
                    // record it — a later shootdown initiated from a healthy
                    // context reports it (see `TlbDiag::report_deferred`).
                    TLB_DIAG
                        .csd_stall_cpu
                        .store(cpu.data() as usize, Ordering::Relaxed);
                    TLB_DIAG.csd_stalls.fetch_add(1, Ordering::Relaxed);
                }
                #[cfg(feature = "mm_selftest")]
                if let Some(observer) = &info.observer {
                    if !observer.skip_ack() {
                        observer.wait_pending(cpu);
                    }
                    test_spins += 1;
                    // Fail-stop rather than leave a live receiver with a
                    // dangling FlushTlbInfo after an unrecoverable timeout.
                    assert!(
                        test_spins < 100_000_000,
                        "selftest TLB acknowledgement timeout"
                    );
                }
                spin_loop();
            }
            // Clean up CSD slot to prevent misuse
            let mut g = csd.info.lock();
            *g = None;
        }
    }

    #[cfg(not(target_arch = "x86_64"))]
    {
        // RISC-V and similar: leverage the underlying synchronous SBI implementation of `send_ipi`
        // (e.g. `remote_sfence_vma`). This path is blocking; return means the target has completed
        // full-mm invalidation, no CSD protocol needed.
        // Note: this path performs a full-mm invalidation without range discrimination,
        // matching the current semantics for this iteration.
        let _ = info; // unused
        for cpu in active.iter_cpu() {
            if cpu == my_cpu {
                continue;
            }
            send_ipi(IpiKind::FlushTLB, IpiTarget::Specified(cpu));
        }
    }
}

/// Deferred, allocation-free diagnostics for the TLB shootdown path.
///
/// Both counters below are recorded **at the site where the anomaly is
/// observed** — which may be an interrupt-disabled / lock-holding context — and
/// printed later by a process-context diagnostics thread. Printing at the
/// observation site would re-introduce the very defect this module is fixing
/// ("console output inside an IRQ-off critical section").
#[derive(Debug, Default)]
pub struct TlbDiag {
    /// Number of shootdowns initiated with interrupts disabled.
    pub irqoff_events: AtomicUsize,
    /// `cpu << 8 | entry` of the most recent such event (`entry`: 0 = mm range, 1 = kernel range).
    pub irqoff_origin: AtomicUsize,
    /// Number of CSD waits that exceeded `CSD_STALL_WARN_SPINS`.
    pub csd_stalls: AtomicUsize,
    /// Target CPU of the most recent stalled CSD wait.
    pub csd_stall_cpu: AtomicUsize,
    /// `file`/`line` of the most recent IRQ-off initiation site (static str).
    pub irqoff_file_ptr: AtomicUsize,
    pub irqoff_file_len: AtomicUsize,
    pub irqoff_line: AtomicUsize,
}

pub static TLB_DIAG: TlbDiag = TlbDiag {
    irqoff_events: AtomicUsize::new(0),
    irqoff_origin: AtomicUsize::new(0),
    csd_stalls: AtomicUsize::new(0),
    csd_stall_cpu: AtomicUsize::new(0),
    irqoff_file_ptr: AtomicUsize::new(0),
    irqoff_file_len: AtomicUsize::new(0),
    irqoff_line: AtomicUsize::new(0),
};

impl TlbDiag {
    /// Print everything recorded since the previous call.
    ///
    /// MUST be called from a context where logging is safe: interrupts enabled
    /// and no lock held that the console path could need. This module calls it
    /// on entry to a shootdown that is itself initiated from such a context, so
    /// a violation recorded in an IRQ-off region is reported by the next healthy
    /// initiator. It is a no-op when nothing new was recorded.
    pub fn report_deferred(&self) {
        static REPORTED_IRQOFF: AtomicUsize = AtomicUsize::new(0);
        static REPORTED_STALLS: AtomicUsize = AtomicUsize::new(0);
        static REPORTED_LINES: AtomicUsize = AtomicUsize::new(0);

        // Bound the output: these anomalies indicate a bug elsewhere and the
        // counters keep counting even after the line budget is spent.
        const MAX_LINES: usize = 16;
        if REPORTED_LINES.load(Ordering::Relaxed) >= MAX_LINES {
            return;
        }

        let irqoff = self.irqoff_events.load(Ordering::Relaxed);
        let reported_irqoff = REPORTED_IRQOFF.swap(irqoff, Ordering::Relaxed);
        if irqoff != reported_irqoff {
            REPORTED_LINES.fetch_add(1, Ordering::Relaxed);
            let origin = self.irqoff_origin.load(Ordering::Relaxed);
            let entry = match origin & 0xff {
                SHOOTDOWN_ENTRY_KERNEL_RANGE => "flush_tlb_kernel_range",
                _ => "flush_tlb_mm_range",
            };
            // The recorded location is the *caller* of the entry point
            // (`#[track_caller]`), i.e. whoever initiated the shootdown while
            // holding a lock or with interrupts disabled.
            let fptr = self.irqoff_file_ptr.load(Ordering::Relaxed);
            let flen = self.irqoff_file_len.load(Ordering::Relaxed);
            let line = self.irqoff_line.load(Ordering::Relaxed);
            let file: &str = if fptr != 0 && flen < 256 {
                unsafe {
                    core::str::from_utf8_unchecked(core::slice::from_raw_parts(
                        fptr as *const u8,
                        flen,
                    ))
                }
            } else {
                "?"
            };
            log::warn!(
                "tlb: {} shootdown(s) initiated with interrupts disabled; last on cpu {} via {} at {}:{}",
                irqoff - reported_irqoff,
                origin >> 8,
                entry,
                file,
                line
            );
        }

        let stalls = self.csd_stalls.load(Ordering::Relaxed);
        let reported_stalls = REPORTED_STALLS.swap(stalls, Ordering::Relaxed);
        if stalls != reported_stalls {
            REPORTED_LINES.fetch_add(1, Ordering::Relaxed);
            log::warn!(
                "tlb: {} CSD wait(s) exceeded the stall bound; last target cpu {}",
                stalls - reported_stalls,
                self.csd_stall_cpu.load(Ordering::Relaxed)
            );
        }
    }
}

/// Verify the context required to *initiate* a synchronous cross-CPU shootdown.
///
/// The initiator must run with interrupts enabled, exactly like Linux
/// `flush_tlb_mm_range()` (which is entered from process context and waits for
/// acknowledgements in a preemption-disabled but interruptible region). Two
/// reasons:
///
/// - While waiting for acknowledgements the initiator itself must be able to
///   take another CPU's `FlushTLB` IPI; otherwise two initiators that both want
///   the (target-owned) CSD slots can wait on each other.
/// - An IRQ taken inside the caller's atomic region can re-enter
///   `__schedule_inner()` and try to take the runqueue lock that the interrupted
///   frame already holds — a self-deadlock that ends in a `self_lock` spinout
///   panic. The previous implementation papered over the "caller had IRQs
///   disabled" case by temporarily enabling interrupts and restoring the
///   caller's state afterwards; that violates the caller's atomicity guarantee
///   and is exactly how the scheduler self-deadlock above can be reached.
///
/// Callers with interrupts disabled are therefore a defect. The violation is
/// recorded here (atomically, no output) and reported by
/// [`TlbDiag::report_deferred`].
#[inline]
#[track_caller]
fn check_shootdown_context(entry_code: usize) {
    #[cfg(target_arch = "x86_64")]
    if !CurrentIrqArch::is_irq_enabled() {
        TLB_DIAG.irqoff_origin.store(
            ((smp_get_processor_id().data() as usize) << 8) | entry_code,
            Ordering::Relaxed,
        );
        let loc = core::panic::Location::caller();
        TLB_DIAG
            .irqoff_file_ptr
            .store(loc.file().as_ptr() as usize, Ordering::Relaxed);
        TLB_DIAG
            .irqoff_file_len
            .store(loc.file().len(), Ordering::Relaxed);
        TLB_DIAG
            .irqoff_line
            .store(loc.line() as usize, Ordering::Relaxed);
        TLB_DIAG.irqoff_events.fetch_add(1, Ordering::Relaxed);
    }
    let _ = entry_code;
}

/// Range-based TLB flush for the specified mm.
///
/// - `start`: start virtual address
/// - `end`: end virtual address (exclusive). If equal to `TLB_FLUSH_ALL`, flush the entire mm.
/// - `stride_shift`: page size shift (typically `MMArch::PAGE_SHIFT`)
/// - `freed_tables`: whether page-table pages were also freed
#[track_caller]
pub fn flush_tlb_mm_range(
    mm: &Arc<AddressSpace>,
    start: VirtAddr,
    end: VirtAddr,
    stride_shift: u8,
    freed_tables: bool,
) {
    check_shootdown_context(SHOOTDOWN_ENTRY_MM_RANGE);
    // Report violations recorded by previous initiators, if this one runs in a
    // context where logging is safe (see `TlbDiag::report_deferred`).
    #[cfg(target_arch = "x86_64")]
    if CurrentIrqArch::is_irq_enabled() {
        TLB_DIAG.report_deferred();
    }

    // Serialize initiators: the per-target CSD slot can only carry one in-flight
    // request. This lock is acquired with interrupts enabled (see
    // `check_shootdown_context`) and disables preemption, so a CPU parked on it
    // can still acknowledge a shootdown addressed to it. Holding it also keeps
    // `info` (a stack object) alive and this CPU pinned for the whole protocol —
    // which is why interrupts no longer need to be disabled here.
    #[cfg(target_arch = "x86_64")]
    let _flush_guard = FLUSH_TLB_GLOBAL_LOCK.lock();

    // Publish barrier: ensure page table writes are visible to other CPUs before the generation increment.
    compiler_fence(Ordering::SeqCst);
    let new_gen = mm.tlb_gen.fetch_add(1, Ordering::SeqCst) + 1;
    compiler_fence(Ordering::SeqCst);

    let this_cpu = smp_get_processor_id();

    let info = FlushTlbInfo {
        mm: Some(mm.clone()),
        start,
        end,
        new_tlb_gen: new_gen,
        stride_shift,
        freed_tables,
        initiating_cpu: this_cpu,
        #[cfg(all(feature = "mm_selftest", target_arch = "x86_64"))]
        observer: selftest::observer_for(mm),
    };

    // Snapshot the remote target set.
    let remote_mask = {
        let g = mm.active_cpus.lock();
        let m = AtomicCpuMask::new();
        for cpu in g.iter_cpu() {
            // Exclude this CPU; local flush is handled separately.
            if cpu != this_cpu {
                m.set(cpu);
            }
        }
        m
    };

    if remote_mask.iter_cpu().next().is_some() {
        flush_tlb_multi(&remote_mask, &info);
    }

    // If this CPU is currently using this mm, perform local flush
    let loaded_local = {
        let st = tlb_state_force(this_cpu);
        st.loaded_is(mm)
    };
    if loaded_local {
        local_flush_tlb_func(&info);
    }

    #[cfg(all(feature = "mm_selftest", target_arch = "x86_64"))]
    if let Some(observer) = &info.observer {
        observer.flush_returned();
    }
    compiler_fence(Ordering::SeqCst);
}

/// Synchronously invalidate a kernel address-space range on every online CPU.
///
/// Kernel mappings are shared by all process page tables, so this request is deliberately
/// independent from any user `AddressSpace` and its TLB generation. Callers must publish PTE
/// changes before invoking this function and must not free intermediate page-table pages until it
/// returns.
#[track_caller]
pub fn flush_tlb_kernel_range(start: VirtAddr, end: VirtAddr, freed_tables: bool) {
    flush_tlb_kernel_range_inner(
        start,
        end,
        freed_tables,
        #[cfg(all(feature = "mm_selftest", target_arch = "x86_64"))]
        None,
    );
}

#[cfg(all(feature = "mm_selftest", target_arch = "x86_64"))]
#[track_caller]
pub(crate) fn flush_tlb_kernel_range_observed(
    start: VirtAddr,
    end: VirtAddr,
    freed_tables: bool,
    observer: Arc<selftest::Observer>,
) {
    flush_tlb_kernel_range_inner(start, end, freed_tables, Some(observer));
}

#[track_caller]
fn flush_tlb_kernel_range_inner(
    start: VirtAddr,
    end: VirtAddr,
    freed_tables: bool,
    #[cfg(all(feature = "mm_selftest", target_arch = "x86_64"))] observer: Option<
        Arc<selftest::Observer>,
    >,
) {
    check_shootdown_context(SHOOTDOWN_ENTRY_KERNEL_RANGE);

    #[cfg(target_arch = "x86_64")]
    let _flush_guard = FLUSH_TLB_GLOBAL_LOCK.lock();

    compiler_fence(Ordering::SeqCst);
    let this_cpu = smp_get_processor_id();
    let info = FlushTlbInfo {
        mm: None,
        start,
        end,
        new_tlb_gen: 0,
        stride_shift: MMArch::PAGE_SHIFT as u8,
        freed_tables,
        initiating_cpu: this_cpu,
        #[cfg(all(feature = "mm_selftest", target_arch = "x86_64"))]
        observer,
    };
    let remote_mask = AtomicCpuMask::new();
    for cpu in smp_cpu_manager().present_cpus().iter_cpu() {
        if cpu != this_cpu {
            remote_mask.set(cpu);
        }
    }
    if remote_mask.iter_cpu().next().is_some() {
        flush_tlb_multi(&remote_mask, &info);
    }
    local_flush_tlb_func(&info);
    #[cfg(all(feature = "mm_selftest", target_arch = "x86_64"))]
    if let Some(observer) = &info.observer {
        observer.flush_returned();
    }
    compiler_fence(Ordering::SeqCst);
}

/// Convenience wrapper for full-mm flush
#[inline]
pub fn flush_tlb_mm(mm: &Arc<AddressSpace>) {
    flush_tlb_mm_range(
        mm,
        VirtAddr::new(0),
        TLB_FLUSH_ALL,
        MMArch::PAGE_SHIFT as u8,
        false,
    );
}
