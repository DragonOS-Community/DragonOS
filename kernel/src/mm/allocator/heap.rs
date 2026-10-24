//! Fixed heap plans and receipts attached to the allocation they actually own.
//! No account lookup, reclaim, or allocation occurs during retirement.
use crate::cgroup::memory::{
    accounting::{ChargeFailure, ChargeFailureReason, KernelByteKind},
    object::{ObjectAccount, ObjectCharge},
    socket::{SockAccount, SockCharge},
};
use crate::mm::allocation::MemoryAllocationError;
#[cfg(feature = "mm_selftest")]
use crate::mm::MemoryManagementArch;
use alloc::{alloc::Global, boxed::Box, collections::VecDeque, sync::Arc, vec::Vec};
use core::{
    alloc::{AllocError, Allocator, Layout},
    mem::MaybeUninit,
    ptr::NonNull,
};
use system_error::SystemError;

#[derive(Debug)]
pub(crate) enum AllocationCharge {
    Object(ObjectCharge),
    Socket(SockCharge),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HeapPlan {
    Physical,
    Virtual,
}

impl HeapPlan {
    pub(crate) const fn prefers_virtual_layout(layout: Layout) -> bool {
        layout.size() > 2 * <crate::arch::MMArch as crate::mm::MemoryManagementArch>::PAGE_SIZE
            || layout.align() > <crate::arch::MMArch as crate::mm::MemoryManagementArch>::PAGE_SIZE
    }

    pub(crate) fn preferred(layout: Layout) -> Self {
        #[cfg(all(
            target_os = "none",
            any(target_arch = "x86_64", target_arch = "riscv64")
        ))]
        if Self::prefers_virtual_layout(layout)
            && crate::mm::vmalloc::global_ready()
            && crate::mm::vmalloc::task_context()
            && crate::mm::vmalloc::global_footprint(layout).is_some()
        {
            return Self::Virtual;
        }
        let _ = layout;
        Self::Physical
    }

    pub(crate) fn footprint(self, layout: Layout) -> u64 {
        if layout.size() == 0 {
            return 0;
        }
        if self == Self::Virtual {
            #[cfg(all(
                target_os = "none",
                any(target_arch = "x86_64", target_arch = "riscv64")
            ))]
            return crate::mm::vmalloc::global_footprint(layout).expect("validated virtual plan");
        }
        super::kernel_allocator::allocation_footprint(layout)
    }

    pub(crate) fn kind(self, requested: KernelByteKind, layout: Layout) -> KernelByteKind {
        if matches!(
            requested,
            KernelByteKind::SlabReclaimable | KernelByteKind::SlabUnreclaimable
        ) {
            if self == Self::Virtual {
                KernelByteKind::Vmalloc
            } else {
                requested.for_allocation(layout)
            }
        } else {
            requested
        }
    }

    /// Execute exactly the admitted plan. A failed plan never silently switches
    /// to a backend with a larger physical footprint.
    pub(crate) unsafe fn allocate(self, layout: Layout) -> Result<NonNull<u8>, AllocError> {
        if layout.size() == 0 {
            return Ok(NonNull::new_unchecked(layout.align() as *mut u8));
        }
        // Fixed plans bypass GlobalAlloc's dispatch, but these are still
        // ordinary Box/Vec allocations for the feature-only no-heap proof.
        #[cfg(feature = "mm_selftest")]
        if crate::mm::oom::denial::is_current_denied() {
            crate::mm::oom::denial::note_rejection();
            return Err(AllocError);
        }
        #[cfg(target_os = "none")]
        {
            #[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
            if self == Self::Virtual {
                return NonNull::new(crate::mm::vmalloc::try_global_alloc(layout))
                    .ok_or(AllocError);
            }
            NonNull::new(crate::KERNEL_ALLOCATOR.alloc_physical(layout, false, true))
                .ok_or(AllocError)
        }
        #[cfg(not(target_os = "none"))]
        {
            Global.allocate(layout).map(|allocation| allocation.cast())
        }
    }
    pub(crate) unsafe fn allocate_admitted(
        self,
        layout: Layout,
        admitted: u64,
    ) -> Result<NonNull<u8>, AllocError> {
        match self.allocate(layout) {
            Ok(pointer) => Ok(pointer),
            Err(_) if self == Self::Virtual && Self::Physical.footprint(layout) <= admitted => {
                Self::Physical.allocate(layout)
            }
            Err(error) => Err(error),
        }
    }
}

pub(crate) fn object_plan(
    owner: &Arc<ObjectAccount>,
    requested: KernelByteKind,
    layout: Layout,
) -> Result<(HeapPlan, ObjectCharge), ChargeFailure> {
    let preferred = HeapPlan::preferred(layout);
    match owner.try_charge(
        preferred.kind(requested, layout),
        preferred.footprint(layout),
    ) {
        Ok(charge) => Ok((preferred, charge)),
        Err(error)
            if error.reason == ChargeFailureReason::Max
                && preferred == HeapPlan::Virtual
                && HeapPlan::Physical.footprint(layout) < preferred.footprint(layout) =>
        {
            let plan = HeapPlan::Physical;
            owner
                .try_charge(plan.kind(requested, layout), plan.footprint(layout))
                .map(|charge| (plan, charge))
        }
        Err(error) => Err(error),
    }
}

pub(crate) fn socket_plan(
    owner: &Arc<SockAccount>,
    layout: Layout,
) -> Result<(HeapPlan, SockCharge), ChargeFailure> {
    let preferred = HeapPlan::preferred(layout);
    match owner.try_charge(preferred.footprint(layout)) {
        Ok(charge) => Ok((preferred, charge)),
        Err(error)
            if error.reason == ChargeFailureReason::Max
                && preferred == HeapPlan::Virtual
                && HeapPlan::Physical.footprint(layout) < preferred.footprint(layout) =>
        {
            owner
                .try_charge(HeapPlan::Physical.footprint(layout))
                .map(|charge| (HeapPlan::Physical, charge))
        }
        Err(error) => Err(error),
    }
}

pub(crate) fn is_virtual(pointer: *mut u8) -> bool {
    #[cfg(all(
        target_os = "none",
        any(target_arch = "x86_64", target_arch = "riscv64")
    ))]
    {
        return crate::mm::vmalloc::contains(crate::mm::VirtAddr::new(pointer as usize));
    }
    #[cfg(not(all(
        target_os = "none",
        any(target_arch = "x86_64", target_arch = "riscv64")
    )))]
    {
        let _ = pointer;
        false
    }
}

pub(crate) unsafe fn charge_existing_object(
    owner: &Arc<ObjectAccount>,
    requested: KernelByteKind,
    pointer: *mut u8,
    layout: Layout,
) -> Result<ObjectCharge, ChargeFailure> {
    let plan = if is_virtual(pointer) {
        HeapPlan::Virtual
    } else {
        HeapPlan::Physical
    };
    owner.try_charge(
        plan.kind(requested, layout),
        actual_footprint(pointer, layout),
    )
}

pub(crate) unsafe fn actual_footprint(pointer: *mut u8, layout: Layout) -> u64 {
    if layout.size() == 0 {
        return 0;
    }
    #[cfg(all(
        target_os = "none",
        any(target_arch = "x86_64", target_arch = "riscv64")
    ))]
    if crate::mm::vmalloc::contains(crate::mm::VirtAddr::new(pointer as usize)) {
        return crate::mm::vmalloc::actual_global_footprint(pointer);
    }
    let _ = pointer;
    HeapPlan::Physical.footprint(layout)
}

pub(crate) unsafe fn finalize_object(
    pointer: *mut u8,
    layout: Layout,
    requested: KernelByteKind,
    charge: &mut ObjectCharge,
) {
    let mut plan = HeapPlan::Physical;
    #[cfg(all(
        target_os = "none",
        any(target_arch = "x86_64", target_arch = "riscv64")
    ))]
    if layout.size() != 0
        && crate::mm::vmalloc::contains(crate::mm::VirtAddr::new(pointer as usize))
    {
        plan = HeapPlan::Virtual;
    }
    charge.shrink_to(
        plan.kind(requested, layout),
        actual_footprint(pointer, layout),
    );
}

/// `pointer/layout` must describe the single allocation owned by `storage`.
/// Its destructor must release that allocation exactly once through GlobalAlloc
/// or the equivalent physical allocator. Values are destroyed in the caller's
/// original context; only backing memory and its receipt may be deferred.
pub(crate) unsafe fn drop_storage_with_charge<T>(
    storage: T,
    pointer: *mut u8,
    layout: Layout,
    mut charge: Option<AllocationCharge>,
) {
    #[cfg(all(
        target_os = "none",
        any(target_arch = "x86_64", target_arch = "riscv64")
    ))]
    if layout.size() != 0
        && crate::mm::vmalloc::contains(crate::mm::VirtAddr::new(pointer as usize))
    {
        if let Some(receipt) = charge.take() {
            crate::mm::vmalloc::attach_charge(pointer, receipt);
        }
    }
    let _ = (pointer, layout);
    drop(storage);
    drop(charge);
}

pub(crate) fn drop_box<T: ?Sized>(storage: Box<T>, charge: Option<AllocationCharge>) {
    let layout = Layout::for_value(&*storage);
    let pointer = (&raw const *storage).cast::<u8>() as *mut u8;
    unsafe { drop_storage_with_charge(storage, pointer, layout, charge) };
}

pub(crate) fn drop_vec<T>(storage: Vec<T>, charge: Option<AllocationCharge>) {
    let layout = Layout::array::<T>(storage.capacity()).expect("valid Vec allocation");
    let pointer = storage.as_ptr() as *mut u8;
    unsafe { drop_storage_with_charge(storage, pointer, layout, charge) };
}

pub(crate) fn drop_deque<T: Copy>(mut storage: VecDeque<T>, charge: Option<AllocationCharge>) {
    storage.clear();
    // From<VecDeque> never reallocates. Clearing Copy values avoids movement
    // of a wrapped ring during last release.
    drop_vec(Vec::from(storage), charge);
}

pub(crate) fn object_vec<T>(
    owner: &Arc<ObjectAccount>,
    kind: KernelByteKind,
    capacity: usize,
) -> Result<(Vec<T>, ObjectCharge), MemoryAllocationError> {
    let layout = Layout::array::<T>(capacity).map_err(|_| SystemError::ENOMEM)?;
    let (plan, mut charge) = object_plan(owner, kind, layout)?;
    let pointer = unsafe { plan.allocate_admitted(layout, charge.bytes()) }
        .map_err(|_| SystemError::ENOMEM)?;
    unsafe { finalize_object(pointer.as_ptr(), layout, kind, &mut charge) };
    Ok((
        unsafe { Vec::from_raw_parts(pointer.as_ptr().cast(), 0, capacity) },
        charge,
    ))
}

pub(crate) unsafe fn uninit_slice<T>(
    plan: HeapPlan,
    len: usize,
    admitted: u64,
) -> Result<Box<[MaybeUninit<T>]>, AllocError> {
    let layout = Layout::array::<T>(len).map_err(|_| AllocError)?;
    let pointer = plan.allocate_admitted(layout, admitted)?;
    Ok(Box::from_raw(core::ptr::slice_from_raw_parts_mut(
        pointer.as_ptr().cast(),
        len,
    )))
}

/// Only PageCache's map backing needs allocator injection. The account remains
/// with its explicit owner; this value controls storage selection only.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) enum HeapPlanAllocator {
    #[default]
    Auto,
    Physical,
}

unsafe impl allocator_api2::alloc::Allocator for HeapPlanAllocator {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, allocator_api2::alloc::AllocError> {
        let pointer = match self {
            Self::Auto => Global.allocate(layout).map(|allocation| allocation.cast()),
            Self::Physical => unsafe { HeapPlan::Physical.allocate(layout) },
        }
        .map_err(|_| allocator_api2::alloc::AllocError)?;
        Ok(NonNull::slice_from_raw_parts(pointer, layout.size()))
    }

    unsafe fn deallocate(&self, pointer: NonNull<u8>, layout: Layout) {
        if layout.size() == 0 {
            return;
        }
        match self {
            Self::Auto => Global.deallocate(pointer, layout),
            Self::Physical => {
                #[cfg(target_os = "none")]
                {
                    use super::kernel_allocator::LocalAlloc;
                    crate::KERNEL_ALLOCATOR.local_dealloc(pointer.as_ptr(), layout);
                }
                #[cfg(not(target_os = "none"))]
                Global.deallocate(pointer, layout);
            }
        }
    }
}

/// Exercise exact quota fallback and the production IRQ retirement worker.
#[cfg(all(
    feature = "mm_selftest",
    any(target_arch = "x86_64", target_arch = "riscv64")
))]
pub(crate) fn selftest() -> bool {
    use crate::cgroup::memory::{
        accounting::{MemoryAccount, MemoryPolicy},
        object::ObjectAccount,
    };
    use crate::libs::spinlock::SpinLock;
    use core::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0x0299_1000_0000_0000);
    fn run() -> Result<bool, SystemError> {
        // Root accounts intentionally ignore memory.max. Use a private leaf
        // so this tests real admission rather than an unlimited root budget.
        let root = MemoryAccount::new(NEXT.fetch_add(1, Ordering::Relaxed), None)?;
        let account = MemoryAccount::new(NEXT.fetch_add(1, Ordering::Relaxed), Some(root.clone()))?;
        // This fixture never participates in controller topology. Keeping its
        // preparation unpublished avoids a global weak link on every report.
        let prepared = ObjectAccount::prepare(&account)?;
        let owner = prepared.object_account();
        let len = 4 * crate::arch::MMArch::PAGE_SIZE;
        let layout = Layout::array::<u8>(len).map_err(|_| SystemError::ENOMEM)?;
        let preferred = HeapPlan::preferred(layout);
        if preferred != HeapPlan::Virtual
            || preferred.footprint(layout) <= HeapPlan::Physical.footprint(layout)
        {
            return Ok(false);
        }
        account.set_policy(MemoryPolicy {
            max: Some(4),
            ..MemoryPolicy::default()
        });
        let (mut bytes, charge) = object_vec::<u8>(owner, KernelByteKind::SlabUnreclaimable, len)
            .map_err(|_| SystemError::ENOMEM)?;
        bytes.resize(len, 0x5a);
        if is_virtual(bytes.as_mut_ptr())
            || account.current_pages() != 4
            || bytes.iter().any(|byte| *byte != 0x5a)
        {
            drop_vec(bytes, Some(AllocationCharge::Object(charge)));
            return Ok(false);
        }
        drop_vec(bytes, Some(AllocationCharge::Object(charge)));
        if account.current_pages() != 0 {
            return Ok(false);
        }
        account.set_policy(MemoryPolicy::default());
        {
            use crate::cgroup::memory::owned::OwnedBox;
            let prepared_box =
                OwnedBox::<[u8; 16384]>::prepare(owner, KernelByteKind::SlabUnreclaimable)
                    .map_err(|_| SystemError::ENOMEM)?;
            let denied = crate::mm::oom::denial::Scope::enter(true);
            let before = crate::mm::oom::denial::rejected_count();
            let zero = object_vec::<()>(owner, KernelByteKind::SlabUnreclaimable, usize::MAX);
            let small = object_vec::<u8>(owner, KernelByteKind::SlabUnreclaimable, 32);
            let large = object_vec::<u8>(owner, KernelByteKind::SlabUnreclaimable, len + 17);
            let boxed = prepared_box.try_allocate();
            let rejected = zero.is_ok()
                && matches!(
                    &small,
                    Err(MemoryAllocationError::System(SystemError::ENOMEM))
                )
                && matches!(
                    &large,
                    Err(MemoryAllocationError::System(SystemError::ENOMEM))
                )
                && boxed.is_err()
                && crate::mm::oom::denial::rejected_count() >= before + 3;
            // Return any unexpectedly admitted storage through its normal
            // receipt-owning release, without weakening the failed assertion.
            for (bytes, receipt) in [small, large].into_iter().flatten() {
                drop_vec(bytes, Some(AllocationCharge::Object(receipt)));
            }
            if let Ok((values, receipt)) = zero {
                drop_vec(values, Some(AllocationCharge::Object(receipt)));
            }
            drop(boxed);
            drop(denied);
            if !rejected || account.current_pages() != 0 || root.current_pages() != 0 {
                return Ok(false);
            }
        }
        let (mut bytes, charge) =
            object_vec::<u8>(owner, KernelByteKind::SlabUnreclaimable, len + 17)
                .map_err(|_| SystemError::ENOMEM)?;
        bytes.resize(len + 17, 0xa5);
        if !is_virtual(bytes.as_mut_ptr()) {
            drop_vec(bytes, Some(AllocationCharge::Object(charge)));
            return Ok(false);
        }
        let irq_lock = SpinLock::new(());
        {
            let _irq = irq_lock.lock_irqsave();
            drop_vec(bytes, Some(AllocationCharge::Object(charge)));
        }
        let deadline = crate::time::timer::next_n_us_timer_jiffies(5_000_000);
        while account.current_pages() != 0 || root.current_pages() != 0 {
            if crate::time::timer::clock() >= deadline {
                return Ok(false);
            }
            crate::sched::sched_yield();
        }
        Ok(root.current_pages() == 0
            && account
                .snapshot()
                .kernel_bytes
                .iter()
                .all(|bytes| *bytes == 0))
    }
    run().unwrap_or(false)
}
