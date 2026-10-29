//! Production allocator entry points exercised with bounded allocations only.
use alloc::vec::Vec;
use core::alloc::{GlobalAlloc, Layout};

use crate::{
    arch::{CurrentIrqArch, MMArch},
    cgroup::memory::{accounting::KernelByteKind, object::ObjectAccount, MemoryAccount},
    exception::InterruptArch,
    mm::{MemoryManagementArch, VirtAddr},
    KERNEL_ALLOCATOR,
};

pub(crate) fn default_heap() -> bool {
    let mut bytes = Vec::<u8>::new();
    if bytes.try_reserve_exact(3 * MMArch::PAGE_SIZE + 17).is_err() {
        return false;
    }
    bytes.resize(3 * MMArch::PAGE_SIZE + 17, 0x69);
    if !crate::mm::vmalloc::contains(VirtAddr::new(bytes.as_ptr() as usize)) {
        return false;
    }
    if bytes.try_reserve_exact(7 * MMArch::PAGE_SIZE).is_err()
        || !bytes.iter().all(|byte| *byte == 0x69)
    {
        return false;
    }
    // The real GlobalAlloc realloc must preserve the original object on failure.
    let layout = Layout::from_size_align(3 * MMArch::PAGE_SIZE, 8192).unwrap();
    unsafe {
        let pointer = KERNEL_ALLOCATOR.alloc_zeroed(layout);
        if pointer.is_null() {
            return false;
        }
        let aligned = (pointer as usize).is_multiple_of(layout.align());
        let zeroed = core::slice::from_raw_parts(pointer, layout.size())
            .iter()
            .all(|byte| *byte == 0);
        core::ptr::write_bytes(pointer, 0x35, layout.size());
        let denied = crate::mm::oom::denial::Scope::enter(true);
        let replacement = KERNEL_ALLOCATOR.realloc(pointer, layout, 7 * MMArch::PAGE_SIZE);
        drop(denied);
        let intact = replacement.is_null()
            && core::slice::from_raw_parts(pointer, layout.size())
                .iter()
                .all(|byte| *byte == 0x35);
        if replacement.is_null() {
            KERNEL_ALLOCATOR.dealloc(pointer, layout);
        } else {
            KERNEL_ALLOCATOR.dealloc(
                replacement,
                Layout::from_size_align(7 * MMArch::PAGE_SIZE, layout.align()).unwrap(),
            );
        }
        aligned && zeroed && intact
    }
}

pub(crate) fn atomic_retirement() -> bool {
    let Ok(account) = MemoryAccount::new(u64::MAX - 80, None) else {
        return false;
    };
    let Ok(prepared) = ObjectAccount::prepare(&account) else {
        return false;
    };
    for disable_irq in [false, true] {
        let Ok((mut bytes, receipt)) = super::heap::object_vec::<u8>(
            prepared.object_account(),
            KernelByteKind::SlabUnreclaimable,
            5 * MMArch::PAGE_SIZE,
        ) else {
            return false;
        };
        bytes.resize(5 * MMArch::PAGE_SIZE, 0xb6);
        if !crate::mm::vmalloc::contains(VirtAddr::new(bytes.as_ptr() as usize)) {
            return false;
        }
        let expected = super::heap::HeapPlan::Virtual
            .footprint(Layout::array::<u8>(bytes.capacity()).unwrap());
        if receipt.bytes() != expected || account.snapshot().current == 0 {
            return false;
        }
        let denied = crate::mm::oom::denial::Scope::enter(true);
        let rejections = crate::mm::oom::denial::rejected_count();
        let preempt = crate::process::preempt::PreemptGuard::new();
        let irq = disable_irq.then(|| unsafe { CurrentIrqArch::save_and_disable_irq() });
        super::heap::drop_vec(bytes, Some(super::heap::AllocationCharge::Object(receipt)));
        drop(irq);
        drop(preempt);
        let allocation_free = crate::mm::oom::denial::rejected_count() == rejections;
        drop(denied);
        if !allocation_free {
            return false;
        }
        // A different CPU may have retired it already. Wait only for this
        // private receipt, never demand quiescence of the global allocator.
        let started = crate::time::timer::clock();
        while account.snapshot().current != 0 {
            if crate::time::timer::clock().wrapping_sub(started) > 5 * crate::time::clocksource::HZ
            {
                return false;
            }
            crate::sched::sched_yield();
        }
    }
    true
}
