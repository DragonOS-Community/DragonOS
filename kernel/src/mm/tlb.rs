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
use core::{
    hint::spin_loop,
    sync::atomic::{AtomicBool, AtomicPtr},
};

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
use crate::libs::spinlock::{SpinLock, SpinLockGuard};

/// Sentinel end value meaning "flush the entire mm"; combined with start=0 it means "do not use range-based flush".
pub const TLB_FLUSH_ALL: VirtAddr = VirtAddr::new(usize::MAX);

/// Cf. Linux `tlb_single_page_flush_ceiling`: above this threshold, fall back to full-mm flush.
pub const TLB_SINGLE_PAGE_FLUSH_CEILING: usize = 33;

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
/// Lock-free publication, claim and acknowledgement protocol. Receivers must
/// remain on the target CPU with interrupts disabled through acknowledgement.
///
/// - 发布方（持有 `FLUSH_TLB_GLOBAL_LOCK`，因此每个目标槽位同时只有一个请求）：
///   1. `done = false`（Release）：先清 ack，保证迟到的 handler 不会提前置位；
///   2. `info = &FlushTlbInfo`（Release）：地址指向发起方栈帧，栈帧在整个等待窗口内有效；
///   3. `compiler_fence(SeqCst)` 后发 IPI。
/// - 接收方（IPI handler / 关中断自旋中的服务路径）：`info.swap(null, AcqRel)`
///   一次性取走指针；取到者执行失效并置 `done = true`，取不到者直接返回。
///   单取者保证不会出现“取走指针后再解引用已失效栈帧”，也避免了同一请求重复 ack。
///
/// 迟到的 IPI 只可能在 `done=false` 且 `info=null`（发布前）或 `info` 已被取走时到达：
/// 前者直接返回（发布方随后会发新的 IPI），后者返回（ack 已由取走者给出）。
/// 若迟到 handler 恰好取走了刚发布但尚未发 IPI 的指针，它会提前完成失效并 ack——
/// 这在语义上仍然正确：页表写入发生在 `done=false`（Release）之前，
/// handler 的 Acquire 读保证能看到新页表，提前失效不会漏掉任何过期 TLB 项。
///
/// x86_64 only; RISC-V's `send_ipi(FlushTLB, ..)` uses synchronous SBI `remote_sfence_vma`
/// and does not need CSD.
#[cfg(target_arch = "x86_64")]
pub struct CsdFlushTlb {
    /// 指向本次请求的 `FlushTlbInfo`（发起方栈上对象）；`null` 表示无请求。
    info: AtomicPtr<FlushTlbInfo>,
    /// 目标 CPU 完成失效后置 `true`。
    done: AtomicBool,
}

// SAFETY: 指针全程由 `info` 的原子交换与 `done` 的 Release/Acquire 配对保护；
// 栈上对象的生命周期覆盖整个等待窗口（见上方协议说明）。
#[cfg(target_arch = "x86_64")]
unsafe impl Sync for CsdFlushTlb {}
#[cfg(target_arch = "x86_64")]
unsafe impl Send for CsdFlushTlb {}

#[cfg(target_arch = "x86_64")]
impl CsdFlushTlb {
    const fn new() -> Self {
        Self {
            info: AtomicPtr::new(core::ptr::null_mut()),
            done: AtomicBool::new(true),
        }
    }

    /// 发布一次请求；调用方必须保证该槽位没有未完成请求。
    fn publish(&self, info: &FlushTlbInfo) {
        self.done.store(false, Ordering::Release);
        self.info.store(
            info as *const FlushTlbInfo as *mut FlushTlbInfo,
            Ordering::Release,
        );
    }

    /// 取走请求指针；只有取到非空值的一方负责执行失效并置 `done`。
    fn take(&self) -> Option<*const FlushTlbInfo> {
        let p = self.info.swap(core::ptr::null_mut(), Ordering::AcqRel);
        if p.is_null() {
            None
        } else {
            Some(p as *const FlushTlbInfo)
        }
    }

    /// 执行已取走的请求（调用方保证 `p` 在 `done` 置位前有效）。
    fn complete(&self, p: *const FlushTlbInfo, from_spin: bool) {
        // SAFETY: 发起方栈帧在 `done=true` 之前一直有效（见协议说明）。
        let info = unsafe { &*p };
        #[cfg(feature = "mm_selftest")]
        if let Some(observer) = &info.observer {
            observer.before_remote(info);
        }
        if from_spin {
            // A contended lock can be inside the CR3/TlbState switch window.
            // Flush hardware conservatively without reading or updating that
            // partially published software state. If CR3 has not switched yet,
            // the subsequent CR3 load also invalidates the incoming mm.
            unsafe { MMArch::invalidate_all() };
        } else {
            local_flush_tlb_func(info);
        }
        #[cfg(feature = "mm_selftest")]
        if let Some(observer) = &info.observer {
            observer.after_remote();
        }
        compiler_fence(Ordering::SeqCst);
        self.done.store(true, Ordering::Release);
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

/// 关中断的等待循环中，主动完成发往本 CPU 的 shootdown。
///
/// 死锁场景：CPU A 持 `FLUSH_TLB_GLOBAL_LOCK` 并等待本 CPU 的 ack，而本 CPU 正
/// 在关中断上下文里自旋争用同一把锁——IPI 无法送达，双方互等。
/// Claim and acknowledge using the IPI protocol, but invalidate hardware in
/// full without accessing CPU-local mm state that the caller may be switching.
#[cfg(target_arch = "x86_64")]
fn service_pending_inbound_flush() {
    // SpinLock's failed try-lock restores IRQ/preemption state before calling
    // the hook. Pin execution from CPU selection through flush and ack, just
    // as the IPI handler does, including when called with IRQs initially on.
    let _irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
    if !crate::smp::cpu::smp_cpu_manager_initialized() {
        return;
    }
    let cpu = smp_get_processor_id();
    let csd = unsafe { csd_flush_tlb().force_get(cpu) };
    if csd.done.load(Ordering::Acquire) {
        return;
    }
    // 一次性取走指针：嵌套调用（例如服务路径执行中又被 IPI 打断）只能有一方
    // 取得指针并置 done，其余直接返回，避免解引用已失效的发起方栈帧。
    // 取走-执行-ack 之间不再持有任何锁，因此也不会与本 CPU 的 IPI handler
    // 形成「handler 自旋等待被它打断的持锁者」的自死锁。
    if let Some(info_ptr) = csd.take() {
        csd.complete(info_ptr, true);
    }
}

/// 获取全局 shootdown 锁；等待期间服务发往本 CPU 的请求，避免 IRQ-off 互等。
#[cfg(target_arch = "x86_64")]
fn acquire_flush_tlb_lock() -> SpinLockGuard<'static, ()> {
    loop {
        if let Ok(guard) = FLUSH_TLB_GLOBAL_LOCK.try_lock() {
            return guard;
        }
        service_pending_inbound_flush();
        core::hint::spin_loop();
    }
}

/// SpinLock contention entry. The service path neither allocates nor takes
/// locks; the atomic claim and IRQ exclusion require no global recursion gate.
#[cfg(target_arch = "x86_64")]
pub fn try_service_inbound_flush_from_spin(_locked_spin: usize) {
    service_pending_inbound_flush();
}

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
        // 让任意关中断自旋等待者都能应答 shootdown（见 libs::spinlock_hook）。
        crate::libs::spinlock_hook::set_hook(try_service_inbound_flush_from_spin);
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
        // Hardware interrupts cannot interrupt the IRQ-off mm switch window.
        let _irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        let cpu = smp_get_processor_id();
        let csd = unsafe { csd_flush_tlb().force_get(cpu) };
        if let Some(info_ptr) = csd.take() {
            csd.complete(info_ptr, false);
        }
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
        // Publish the request into each target's CSD slot. Ordering matters:
        // clear `done` before storing `info`, so a late IPI from a previous
        // shootdown can never observe the new pointer with a stale ack.
        for cpu in active.iter_cpu() {
            if cpu == my_cpu {
                continue;
            }
            let csd = unsafe { csd_flush_tlb().force_get(cpu) };
            csd.publish(info);
        }

        // 发布与 IPI 之间不允许编译器重排（见 `CsdFlushTlb::publish` 的顺序要求）。
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

        // Poll for done
        for cpu in active.iter_cpu() {
            if cpu == my_cpu {
                continue;
            }
            let csd = unsafe { csd_flush_tlb().force_get(cpu) };
            #[cfg(feature = "mm_selftest")]
            let mut test_spins = 0usize;
            let mut spins = 0u64;
            while !csd.done.load(Ordering::Acquire) {
                // 本 CPU 可能在等锁的其它 CPU 眼里也是目标；周期性服务发往
                // 本 CPU 的请求，避免双方都在关中断等待时互等（见
                // service_pending_inbound_flush 的说明）。
                spins += 1;
                if spins & 1023 == 0 {
                    service_pending_inbound_flush();
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
            // No cleanup needed: the receiver already swapped the pointer out
            // (`take`), so the slot is empty once `done` is observed.
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
    // Keep the caller's IRQ state throughout lock contention and initiation.
    // Waiting CPUs explicitly service inbound requests without enabling IRQs
    // inside an unrelated caller-owned critical section.
    let _irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
    #[cfg(target_arch = "x86_64")]
    let _flush_guard = acquire_flush_tlb_lock();

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
pub(crate) fn flush_tlb_kernel_range_observed(
    start: VirtAddr,
    end: VirtAddr,
    freed_tables: bool,
    observer: Arc<selftest::Observer>,
) {
    flush_tlb_kernel_range_inner(start, end, freed_tables, Some(observer));
}

fn flush_tlb_kernel_range_inner(
    start: VirtAddr,
    end: VirtAddr,
    freed_tables: bool,
    #[cfg(all(feature = "mm_selftest", target_arch = "x86_64"))] observer: Option<
        Arc<selftest::Observer>,
    >,
) {
    let _irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
    #[cfg(target_arch = "x86_64")]
    let _flush_guard = acquire_flush_tlb_lock();

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
