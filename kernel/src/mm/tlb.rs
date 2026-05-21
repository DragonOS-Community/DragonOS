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

use core::sync::atomic::{compiler_fence, Ordering};

#[cfg(target_arch = "x86_64")]
use core::{hint::spin_loop, sync::atomic::AtomicBool};

use alloc::sync::Arc;
#[allow(unused_imports)]
use alloc::vec::Vec;

use crate::{
    arch::{interrupt::ipi::send_ipi, CurrentIrqArch, MMArch},
    exception::{
        ipi::{IpiKind, IpiTarget},
        InterruptArch,
    },
    libs::cpumask::CpuMask,
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

/// Context for a single shootdown. Lives on the initiator's stack, spanning the entire IPI wait window.
#[derive(Debug)]
#[allow(dead_code)]
pub struct FlushTlbInfo {
    /// Target address space
    pub mm: Arc<AddressSpace>,
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
    let loaded_matches = {
        let st = tlb_state_force(smp_get_processor_id());
        st.loaded_is(&info.mm)
    };

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

    let st = tlb_state_local_mut();
    if st.loaded_tlb_gen < info.new_tlb_gen {
        st.loaded_tlb_gen = info.new_tlb_gen;
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

        local_flush_tlb_func(info);

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
fn flush_tlb_multi(target_cpus: &CpuMask, info: &FlushTlbInfo) {
    // Filter out offline CPUs to avoid sending IPIs to not-yet-started / offline APICs.
    let online = smp_cpu_manager().present_cpus();
    let active: CpuMask = target_cpus & online;

    let my_cpu = smp_get_processor_id();

    #[cfg(target_arch = "x86_64")]
    {
        // Global lock serialization: ensures only one shootdown uses per-CPU CSD slots at a time.
        // Can be refined to per-target dedicated channels in the future.
        let _g = FLUSH_TLB_GLOBAL_LOCK.lock();

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

        // Poll for done
        for cpu in active.iter_cpu() {
            if cpu == my_cpu {
                continue;
            }
            let csd = unsafe { csd_flush_tlb().force_get(cpu) };
            while !csd.done.load(Ordering::Acquire) {
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

/// Range-based TLB flush for the specified mm.
///
/// - `start`: start virtual address
/// - `end`: end virtual address (exclusive). If equal to `TLB_FLUSH_ALL`, flush the entire mm.
/// - `stride_shift`: page size shift (typically `MMArch::PAGE_SHIFT`)
/// - `freed_tables`: whether page-table pages were also freed
pub fn flush_tlb_mm_range(
    mm: &Arc<AddressSpace>,
    start: VirtAddr,
    end: VirtAddr,
    stride_shift: u8,
    freed_tables: bool,
) {
    // Disable interrupts to protect the entire initiation process:
    // - Prevent migration during inc_tlb_gen and snapshot of active_cpus;
    // - Prevent being scheduled out while waiting for IPI ack.
    let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };

    // Publish barrier: ensure page table writes are visible to other CPUs before the generation increment.
    compiler_fence(Ordering::SeqCst);
    let new_gen = mm.tlb_gen.fetch_add(1, Ordering::SeqCst) + 1;
    compiler_fence(Ordering::SeqCst);

    let this_cpu = smp_get_processor_id();

    let info = FlushTlbInfo {
        mm: mm.clone(),
        start,
        end,
        new_tlb_gen: new_gen,
        stride_shift,
        freed_tables,
        initiating_cpu: this_cpu,
    };

    // Snapshot the remote target set.
    let remote_mask: CpuMask = {
        let g = mm.active_cpus.lock();
        let mut m = g.clone();
        // Exclude this CPU; local flush is handled separately
        m.set(this_cpu, false);
        m
    };

    if !remote_mask.is_empty() {
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

    compiler_fence(Ordering::SeqCst);
    drop(irq_guard);
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
