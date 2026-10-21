//! Fixed, zeroed CPU scratch storage. Callers may assume virtual continuity
//! only; devices must use their own DMA ownership/mapping API.
//!
//! Construction AND destruction require preemptible task context, IRQs on,
//! and no MM/allocator locks. No implicit reclaim or operation retries occur.

use alloc::sync::Arc;
use core::{
    alloc::Layout,
    ops::{Deref, DerefMut},
    ptr::NonNull,
};
use system_error::SystemError;

use crate::{
    arch::MMArch,
    cgroup::memory::{
        accounting::KernelByteKind,
        object::{ObjectAccount, ObjectCharge},
        object_for_task,
    },
    mm::{
        allocation::MemoryAllocationError,
        allocator::kernel_allocator::LocalAlloc,
        vmalloc::{self, VmAllocation},
        MemoryManagementArch,
    },
    process::ProcessManager,
    KERNEL_ALLOCATOR,
};

#[cfg(feature = "mm_selftest")]
type VirtualAllocate<'a> =
    dyn FnMut(Layout, &Arc<ObjectAccount>) -> Result<VmAllocation, MemoryAllocationError> + 'a;

enum Backend {
    Empty,
    Heap {
        pointer: NonNull<u8>,
        layout: Layout,
        _charge: ObjectCharge,
    },
    Virtual(VmAllocation),
}

/// Unique, fixed-size bytes. It deliberately has no Send/Sync implementation:
/// audited local scratch owners must not escape into IRQ/last-Arc-drop paths.
pub(crate) struct KernelBuffer {
    backend: Backend,
}

impl KernelBuffer {
    /// Prepare local scratch at a resource-lock-free task boundary. Only the
    /// unpublished allocation is retried; callers must perform I/O afterwards.
    pub(crate) fn new_zeroed_recovering(len: usize) -> Result<Self, SystemError> {
        let mut recovery = crate::cgroup::memory::runtime::ChargeRecoveryState::default();
        loop {
            match Self::try_new_zeroed(len) {
                Ok(buffer) => return Ok(buffer),
                Err(error) => error.recover(&mut recovery)?,
            }
        }
    }

    pub(crate) fn try_new_zeroed(len: usize) -> Result<Self, MemoryAllocationError> {
        if !vmalloc::task_context() {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK.into());
        }
        let owner = object_for_task(&ProcessManager::current_pcb());
        Self::allocate_in(
            len,
            &owner,
            VmAllocation::is_supported(),
            #[cfg(feature = "mm_selftest")]
            false,
            #[cfg(feature = "mm_selftest")]
            &mut VmAllocation::try_new_zeroed_in,
        )
    }

    #[cfg(feature = "mm_selftest")]
    pub(crate) fn try_new_zeroed_in(
        len: usize,
        owner: &Arc<ObjectAccount>,
    ) -> Result<Self, MemoryAllocationError> {
        Self::allocate_in(
            len,
            owner,
            VmAllocation::is_supported(),
            false,
            &mut VmAllocation::try_new_zeroed_in,
        )
    }

    /// Controlled local backing for consumers testing the real selection and
    /// mapping path. Successful frames transfer to VmAllocation's normal Drop.
    #[cfg(feature = "mm_selftest")]
    pub(crate) fn try_new_zeroed_in_with(
        len: usize,
        owner: &Arc<ObjectAccount>,
        data_frame: &mut dyn FnMut() -> Result<super::PhysAddr, SystemError>,
    ) -> Result<Self, MemoryAllocationError> {
        Self::allocate_in(
            len,
            owner,
            VmAllocation::is_supported(),
            false,
            &mut |layout, selected| {
                VmAllocation::try_new_zeroed_in_with(layout, selected, data_frame)
            },
        )
    }

    fn allocate_in(
        len: usize,
        owner: &Arc<ObjectAccount>,
        virtual_supported: bool,
        #[cfg(feature = "mm_selftest")] fail_heap: bool,
        #[cfg(feature = "mm_selftest")] virtual_allocate: &mut VirtualAllocate<'_>,
    ) -> Result<Self, MemoryAllocationError> {
        if !vmalloc::task_context() {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK.into());
        }
        if len == 0 {
            return Ok(Self {
                backend: Backend::Empty,
            });
        }
        let layout = Layout::array::<u8>(len).map_err(|_| SystemError::ENOMEM)?;
        // When virtual mappings are supported, requests above order-1 bypass
        // high-order pressure and rounding waste. Otherwise retain the heap
        // backend for all sizes, with the same admission and no-retry policy.
        if len <= 2 * MMArch::PAGE_SIZE || !virtual_supported {
            let charge = owner.try_charge_allocation(KernelByteKind::SlabUnreclaimable, layout)?;
            #[cfg(feature = "mm_selftest")]
            let pointer = if fail_heap {
                core::ptr::null_mut()
            } else {
                unsafe { KERNEL_ALLOCATOR.alloc_buffer_zeroed(layout) }
            };
            #[cfg(not(feature = "mm_selftest"))]
            let pointer = unsafe { KERNEL_ALLOCATOR.alloc_buffer_zeroed(layout) };
            if let Some(pointer) = NonNull::new(pointer) {
                return Ok(Self {
                    backend: Backend::Heap {
                        pointer,
                        layout,
                        _charge: charge,
                    },
                });
            }
            // Never have both backends charged at once. A charge rejection
            // above is propagated, not converted into a fallback opportunity.
            drop(charge);
            if len <= MMArch::PAGE_SIZE || !virtual_supported {
                return Err(SystemError::ENOMEM.into());
            }
        }
        #[cfg(feature = "mm_selftest")]
        let allocation = virtual_allocate(layout, owner)?;
        #[cfg(not(feature = "mm_selftest"))]
        let allocation = VmAllocation::try_new_zeroed_in(layout, owner)?;
        Ok(Self {
            backend: Backend::Virtual(allocation),
        })
    }
}

impl Deref for KernelBuffer {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match &self.backend {
            Backend::Empty => &[],
            Backend::Heap {
                pointer, layout, ..
            } => unsafe { core::slice::from_raw_parts(pointer.as_ptr(), layout.size()) },
            Backend::Virtual(allocation) => allocation.as_slice(),
        }
    }
}

impl DerefMut for KernelBuffer {
    fn deref_mut(&mut self) -> &mut [u8] {
        match &mut self.backend {
            Backend::Empty => &mut [],
            Backend::Heap {
                pointer, layout, ..
            } => unsafe { core::slice::from_raw_parts_mut(pointer.as_ptr(), layout.size()) },
            Backend::Virtual(allocation) => allocation.as_mut_slice(),
        }
    }
}

impl Drop for KernelBuffer {
    fn drop(&mut self) {
        assert!(
            vmalloc::task_context(),
            "CPU buffer freed outside task context"
        );
        if let Backend::Heap {
            pointer, layout, ..
        } = &self.backend
        {
            // The enum's charge is dropped only after this physical release.
            unsafe { KERNEL_ALLOCATOR.local_dealloc(pointer.as_ptr(), *layout) };
        }
        // Virtual uses VmAllocation's unmap -> TLB completion -> free -> uncharge.
    }
}

#[cfg(feature = "mm_selftest")]
mod selftest;
#[cfg(feature = "mm_selftest")]
pub(crate) use selftest::report as selftest_report;
