//! Fixed, zeroed CPU scratch storage. Callers may assume virtual continuity
//! only; devices must use their own DMA ownership/mapping API.
//!
//! Construction AND destruction require preemptible task context, IRQs on,
//! and no MM/allocator locks. No implicit reclaim or operation retries occur.

use alloc::sync::Arc;
use core::{
    alloc::Layout,
    marker::PhantomData,
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
        let layout = Layout::array::<u8>(len).map_err(|_| SystemError::ENOMEM)?;
        Self::try_new_zeroed_layout(layout)
    }

    fn try_new_zeroed_layout(layout: Layout) -> Result<Self, MemoryAllocationError> {
        if !vmalloc::task_context() {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK.into());
        }
        let owner = object_for_task(&ProcessManager::current_pcb());
        Self::allocate_layout_in(
            layout,
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

    #[cfg(feature = "mm_selftest")]
    fn allocate_in(
        len: usize,
        owner: &Arc<ObjectAccount>,
        virtual_supported: bool,
        #[cfg(feature = "mm_selftest")] fail_heap: bool,
        #[cfg(feature = "mm_selftest")] virtual_allocate: &mut VirtualAllocate<'_>,
    ) -> Result<Self, MemoryAllocationError> {
        let layout = Layout::array::<u8>(len).map_err(|_| SystemError::ENOMEM)?;
        Self::allocate_layout_in(
            layout,
            owner,
            virtual_supported,
            #[cfg(feature = "mm_selftest")]
            fail_heap,
            #[cfg(feature = "mm_selftest")]
            virtual_allocate,
        )
    }

    fn allocate_layout_in(
        layout: Layout,
        owner: &Arc<ObjectAccount>,
        virtual_supported: bool,
        #[cfg(feature = "mm_selftest")] fail_heap: bool,
        #[cfg(feature = "mm_selftest")] virtual_allocate: &mut VirtualAllocate<'_>,
    ) -> Result<Self, MemoryAllocationError> {
        if !vmalloc::task_context() {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK.into());
        }
        let len = layout.size();
        if len == 0 {
            return Ok(Self {
                backend: Backend::Empty,
            });
        }
        // The heap route chooses buddy order from size alone. Do not pass
        // over-page alignment to it, even for a tiny Layout.
        let heap_supported = layout.align() <= MMArch::PAGE_SIZE;
        if !heap_supported && !virtual_supported {
            return Err(SystemError::ENOMEM.into());
        }
        // When virtual mappings are supported, requests above order-1 bypass
        // high-order pressure and rounding waste. Otherwise retain the heap
        // backend for all sizes, with the same admission and no-retry policy.
        if heap_supported && (len <= 2 * MMArch::PAGE_SIZE || !virtual_supported) {
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

    // Borrow only the address: a typed owner may have written T values whose
    // padding is uninitialized, so it must not materialize a byte slice.
    fn pointer(&self) -> NonNull<u8> {
        match &self.backend {
            Backend::Empty => NonNull::dangling(),
            Backend::Heap { pointer, .. } => *pointer,
            Backend::Virtual(allocation) => {
                NonNull::new(allocation.address().as_ptr::<u8>()).unwrap()
            }
        }
    }
}

/// Fixed CPU-only objects with task-local allocation and destruction. Copy
/// keeps initialization and teardown simple; storage never grows or escapes as
/// a Box/Vec, and inherits KernelBuffer's non-Send/non-Sync ownership.
/// Both creation and final drop require the same context as KernelBuffer.
pub(crate) struct KernelSlice<T: Copy> {
    storage: KernelBuffer,
    len: usize,
    _element: PhantomData<T>,
}

impl<T: Copy> KernelSlice<T> {
    pub(crate) fn try_filled(len: usize, initial: T) -> Result<Self, MemoryAllocationError> {
        let layout = Layout::array::<T>(len).map_err(|_| SystemError::ENOMEM)?;
        let storage = KernelBuffer::try_new_zeroed_layout(layout)?;
        Ok(Self::initialize(storage, len, initial))
    }

    fn initialize(storage: KernelBuffer, len: usize, initial: T) -> Self {
        // Only callers that allocated Layout::array::<T>(len) enter here.
        // Use a valid value, never assume that zero is a valid T (e.g. NonZero).
        if core::mem::size_of::<T>() != 0 {
            let pointer = storage.pointer().as_ptr().cast::<T>();
            for index in 0..len {
                unsafe { pointer.add(index).write(initial) };
            }
        }
        Self {
            storage,
            len,
            _element: PhantomData,
        }
    }

    fn pointer(&self) -> NonNull<T> {
        if self.len == 0 || core::mem::size_of::<T>() == 0 {
            NonNull::dangling()
        } else {
            self.storage.pointer().cast()
        }
    }
}

impl<T: Copy> Deref for KernelSlice<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        // initialize wrote every non-ZST element from a valid Copy value;
        // an inhabited ZST needs no bytes, including when its length is huge.
        unsafe { core::slice::from_raw_parts(self.pointer().as_ptr(), self.len) }
    }
}

impl<T: Copy> DerefMut for KernelSlice<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        unsafe { core::slice::from_raw_parts_mut(self.pointer().as_ptr(), self.len) }
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
