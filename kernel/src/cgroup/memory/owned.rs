//! Fallible admission for a uniquely owned heap allocation.
//!
//! Admission never performs reclaim, OOM or waiting. Fast preparation may run
//! inside an audited ordinary mutex transaction; recovery belongs outside its
//! guards. IRQ, MM and runqueue locks must not acquire new owned storage. This
//! does not charge Arc handles or change the global allocator's policy.

use alloc::{boxed::Box, sync::Arc};
use core::{
    alloc::{AllocError, Layout},
    marker::PhantomData,
    mem::MaybeUninit,
    ops::{Deref, DerefMut},
};

use super::{
    accounting::{ChargeFailure, KernelByteKind},
    object::{ObjectAccount, ObjectCharge},
};
#[cfg(test)]
use crate::mm::allocator::kernel_allocator::allocation_footprint;

#[derive(Debug)]
#[must_use]
pub(crate) struct PreparedBox<T> {
    charge: ObjectCharge,
    value: PhantomData<T>,
}

#[derive(Debug)]
pub(crate) struct OwnedBox<T> {
    value: Option<Box<T>>,
    charge: Option<ObjectCharge>,
}

/// Actual storage prepared before a constructor acquires external resources.
/// It never exposes uninitialized T and releases storage before its receipt.
#[derive(Debug)]
#[must_use]
pub(crate) struct AllocatedBox<T> {
    value: Option<Box<MaybeUninit<T>>>,
    charge: Option<ObjectCharge>,
}

impl<T> OwnedBox<T> {
    /// Borrow the entity address for an unpublished constructor's raw field
    /// initialization. Ownership and the charge receipt stay in this Box.
    ///
    /// # Safety
    /// The caller must exclusively own the unpublished entity, with no live
    /// references to this slot or its T (including guards derived from T).
    /// The pointer must not outlive the Box, escape to concurrent users, or
    /// be used to free/move T. Only the value slot is borrowed, not the whole
    /// wrapper or T. The primitive Box dereference materializes no T reference.
    pub(crate) unsafe fn unpublished_value_ptr(this: *mut Self) -> *mut T {
        let slot = unsafe { &mut *core::ptr::addr_of_mut!((*this).value) };
        let boxed = slot.as_mut().expect("unpublished owned value is present");
        &raw mut **boxed
    }

    pub fn prepare(
        account: &Arc<ObjectAccount>,
        kind: KernelByteKind,
    ) -> Result<PreparedBox<T>, ChargeFailure> {
        let charge = account.try_charge_allocation(kind, Layout::new::<T>())?;
        Ok(PreparedBox {
            charge,
            value: PhantomData,
        })
    }
}

/// A fixed physical allocation with explicit ownership. Slice borrowers may
/// mutate elements, but cannot grow storage or detach its admission receipt.
#[derive(Debug)]
pub(crate) struct OwnedSlice<T: Copy> {
    storage: Option<Box<[T]>>,
    charge: Option<ObjectCharge>,
}

impl<T: Copy> OwnedSlice<T> {
    /// Fast admission only. Recover typed failures in the caller's resource-
    /// lock-free operation, never inside this constructor or an IRQ helper.
    pub(crate) fn try_filled(
        account: &Arc<ObjectAccount>,
        kind: KernelByteKind,
        len: usize,
        initial: T,
    ) -> Result<Self, crate::mm::allocation::MemoryAllocationError> {
        let layout = Layout::array::<T>(len).map_err(|_| system_error::SystemError::ENOMEM)?;
        let charge = if layout.size() == 0 {
            None
        } else {
            Some(account.try_charge_allocation(kind, layout)?)
        };
        let mut storage =
            Box::<[T]>::try_new_uninit_slice(len).map_err(|_| system_error::SystemError::ENOMEM)?;
        if core::mem::size_of::<T>() != 0 {
            for slot in storage.iter_mut() {
                slot.write(initial);
            }
        }
        // Non-ZST elements were each initialized from a valid Copy value.
        // A valid `initial` proves a ZST is inhabited; its elements contain no
        // storage bytes to initialize. This also avoids a huge empty loop.
        let storage = unsafe { storage.assume_init() };
        Ok(Self {
            storage: Some(storage),
            charge,
        })
    }
}

impl<T: Copy> Deref for OwnedSlice<T> {
    type Target = [T];
    fn deref(&self) -> &Self::Target {
        self.storage
            .as_deref()
            .expect("owned slice storage is present")
    }
}

impl<T: Copy> DerefMut for OwnedSlice<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.storage
            .as_deref_mut()
            .expect("owned slice storage is present")
    }
}

impl<T: Copy> Drop for OwnedSlice<T> {
    fn drop(&mut self) {
        drop(self.storage.take());
        drop(self.charge.take());
    }
}

impl<T> PreparedBox<T> {
    /// Physical ENOMEM is distinct from the typed admission failure above.
    /// If allocation fails, the prepared receipt is rolled back by RAII.
    pub fn try_init(self, value: T) -> Result<OwnedBox<T>, AllocError> {
        let storage = match Box::<T>::try_new_uninit() {
            Ok(storage) => storage,
            Err(error) => {
                // Keep the receipt alive while releasing the caller's value,
                // just as Box::try_new(value) does on allocation failure.
                drop(value);
                return Err(error);
            }
        };
        Ok(AllocatedBox {
            value: Some(storage),
            charge: Some(self.charge),
        }
        .init(value))
    }

    /// Allocate before open-like side effects, leaving final initialization
    /// infallible. MaybeUninit<T> has exactly T's size and alignment.
    pub fn try_allocate(self) -> Result<AllocatedBox<T>, AllocError> {
        let value = Box::<T>::try_new_uninit()?;
        Ok(AllocatedBox {
            value: Some(value),
            charge: Some(self.charge),
        })
    }
}

impl<T> AllocatedBox<T> {
    pub fn init(mut self, value: T) -> OwnedBox<T> {
        let mut storage = self
            .value
            .take()
            .expect("allocated Box is present until init");
        storage.write(value);
        OwnedBox {
            // SAFETY: write above initialized the entire T. No uninitialized
            // reference escapes and this transfers the unique Box once.
            value: Some(unsafe { storage.assume_init() }),
            charge: self.charge.take(),
        }
    }
}

impl<T, const N: usize> AllocatedBox<[T; N]> {
    /// Initialize a large owned array directly in its prepared allocation.
    /// Only an element-sized temporary is needed; a panicking initializer
    /// releases the initialized prefix before storage and its receipt.
    pub fn init_array(mut self, mut initialize: impl FnMut(usize) -> T) -> OwnedBox<[T; N]> {
        struct Prefix<'a, T> {
            elements: &'a mut [MaybeUninit<T>],
            initialized: usize,
        }
        impl<T> Drop for Prefix<'_, T> {
            fn drop(&mut self) {
                for element in &mut self.elements[..self.initialized] {
                    // SAFETY: initialized only advances after write succeeds.
                    unsafe { element.assume_init_drop() };
                }
            }
        }
        let mut storage = self.value.take().expect("prepared array allocation");
        // SAFETY: a prepared [T; N] allocation has the same layout as N
        // MaybeUninit<T> elements; the exclusive slice never escapes this call.
        let elements = unsafe {
            core::slice::from_raw_parts_mut(storage.as_mut_ptr().cast::<MaybeUninit<T>>(), N)
        };
        let mut prefix = Prefix {
            elements,
            initialized: 0,
        };
        for index in 0..N {
            prefix.elements[index].write(initialize(index));
            prefix.initialized += 1;
        }
        prefix.initialized = 0;
        drop(prefix);
        OwnedBox {
            // SAFETY: all N elements were written; prefix no longer drops them.
            value: Some(unsafe { storage.assume_init() }),
            charge: self.charge.take(),
        }
    }
}

impl<T> Drop for AllocatedBox<T> {
    fn drop(&mut self) {
        drop(self.value.take());
        drop(self.charge.take());
    }
}

impl<T> Deref for OwnedBox<T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.value
            .as_deref()
            .expect("owned Box is present until drop")
    }
}

impl<T> DerefMut for OwnedBox<T> {
    fn deref_mut(&mut self) -> &mut T {
        self.value
            .as_deref_mut()
            .expect("owned Box is present until drop")
    }
}

impl<T> Drop for OwnedBox<T> {
    fn drop(&mut self) {
        // Returning the Box allocation precedes uncharging. Keeping a receipt
        // in T itself would instead uncharge before Box deallocation.
        drop(self.value.take());
        drop(self.charge.take());
    }
}

#[cfg(test)]
mod tests {
    use super::super::accounting::{MemoryAccount, MemoryPolicy};
    use super::*;

    fn account() -> (Arc<MemoryAccount>, Arc<ObjectAccount>) {
        let account = MemoryAccount::new(1, None).unwrap();
        let object = ObjectAccount::new(&account).unwrap();
        (account, object)
    }

    #[test]
    fn fixed_slice_preserves_storage_and_actual_kernel_class() {
        let (account, object) = account();
        let mut small =
            OwnedSlice::try_filled(&object, KernelByteKind::SlabUnreclaimable, 2048, 3u8).unwrap();
        let large =
            OwnedSlice::try_filled(&object, KernelByteKind::SlabUnreclaimable, 4096, 7u8).unwrap();
        assert!(small.iter().all(|byte| *byte == 3));
        assert!(large.iter().all(|byte| *byte == 7));
        let original = small.as_ptr();
        small[100] = 9;
        assert_eq!(small.as_ptr(), original);
        assert_eq!(small[100], 9);
        let snapshot = account.snapshot();
        assert_eq!(
            snapshot.kernel_bytes[KernelByteKind::SlabUnreclaimable as usize],
            2048
        );
        assert_eq!(
            snapshot.kernel_bytes[KernelByteKind::KernelOther as usize],
            4096
        );
        drop((small, large));
        assert_eq!(account.current_pages(), 0);
        assert_eq!(
            account.snapshot().kernel_bytes,
            [0; super::super::accounting::KERNEL_BYTE_KIND_COUNT]
        );
    }

    #[test]
    fn empty_and_large_zst_slices_do_not_charge_a_zero_limit_leaf() {
        let root = MemoryAccount::new(21, None).unwrap();
        let leaf = MemoryAccount::new(22, Some(root)).unwrap();
        leaf.set_policy(MemoryPolicy {
            max: Some(0),
            ..MemoryPolicy::default()
        });
        let object = ObjectAccount::new(&leaf).unwrap();
        let empty =
            OwnedSlice::try_filled(&object, KernelByteKind::SlabUnreclaimable, 0, 7u8).unwrap();
        let zst =
            OwnedSlice::try_filled(&object, KernelByteKind::SlabUnreclaimable, usize::MAX, ())
                .unwrap();
        assert_eq!(empty.len(), 0);
        assert_eq!(zst.len(), usize::MAX);
        assert_eq!(leaf.current_pages(), 0);
        drop((empty, zst));
        assert_eq!(
            leaf.snapshot().kernel_bytes,
            [0; super::super::accounting::KERNEL_BYTE_KIND_COUNT]
        );
    }

    #[test]
    fn fixed_slice_admission_and_invalid_layout_leave_no_inventory() {
        let root = MemoryAccount::new(23, None).unwrap();
        let leaf = MemoryAccount::new(24, Some(root)).unwrap();
        leaf.set_policy(MemoryPolicy {
            max: Some(0),
            ..MemoryPolicy::default()
        });
        let object = ObjectAccount::new(&leaf).unwrap();
        assert!(matches!(
            OwnedSlice::try_filled(&object, KernelByteKind::SlabUnreclaimable, 16, 0u8),
            Err(crate::mm::allocation::MemoryAllocationError::Charge(_))
        ));
        assert!(matches!(
            OwnedSlice::try_filled(&object, KernelByteKind::SlabUnreclaimable, usize::MAX, 0u8),
            Err(crate::mm::allocation::MemoryAllocationError::System(
                system_error::SystemError::ENOMEM
            ))
        ));
        assert_eq!(leaf.current_pages(), 0);
        assert_eq!(
            leaf.snapshot().kernel_bytes,
            [0; super::super::accounting::KERNEL_BYTE_KIND_COUNT]
        );
    }

    #[test]
    fn array_initializes_actual_page_storage_in_place() {
        let (account, object) = account();
        let storage = OwnedBox::<[usize; 512]>::prepare(&object, KernelByteKind::SlabUnreclaimable)
            .unwrap()
            .try_allocate()
            .unwrap();
        let array = storage.init_array(|index| index + 1);
        for index in 0..512 {
            assert_eq!(array[index], index + 1);
        }
        assert_eq!(account.snapshot().current, 1);
        drop(array);
        assert_eq!(account.snapshot().current, 0);
        assert_eq!(
            account.snapshot().kernel_bytes,
            [0; crate::cgroup::memory::accounting::KERNEL_BYTE_KIND_COUNT]
        );
    }

    #[test]
    fn real_slab_and_buddy_storage_have_distinct_statistics() {
        #[repr(align(4096))]
        struct Aligned([u8; 1]);
        let (account, object) = account();
        let small = OwnedBox::<[u8; 2048]>::prepare(&object, KernelByteKind::SlabUnreclaimable)
            .unwrap()
            .try_allocate()
            .unwrap()
            .init([0; 2048]);
        let large = OwnedBox::<[u8; 2064]>::prepare(&object, KernelByteKind::SlabUnreclaimable)
            .unwrap()
            .try_allocate()
            .unwrap()
            .init([0; 2064]);
        let aligned = OwnedBox::<Aligned>::prepare(&object, KernelByteKind::SlabReclaimable)
            .unwrap()
            .try_allocate()
            .unwrap()
            .init(Aligned([7]));
        assert_eq!(aligned.0[0], 7);
        let snapshot = account.snapshot();
        assert_eq!(
            snapshot.kernel_bytes[KernelByteKind::SlabUnreclaimable as usize],
            2048
        );
        assert_eq!(
            snapshot.kernel_bytes[KernelByteKind::SlabReclaimable as usize],
            0
        );
        assert_eq!(
            snapshot.kernel_bytes[KernelByteKind::KernelOther as usize],
            8192
        );
        assert_eq!(snapshot.current, 3);
        assert_eq!(
            KernelByteKind::Stack.for_allocation(Layout::new::<Aligned>()),
            KernelByteKind::Stack
        );
        assert_eq!(
            KernelByteKind::PageTables.for_allocation(Layout::new::<Aligned>()),
            KernelByteKind::PageTables
        );
        drop((small, large, aligned));
        assert_eq!(account.current_pages(), 0);
        assert_eq!(
            account.snapshot().kernel_bytes,
            [0; crate::cgroup::memory::accounting::KERNEL_BYTE_KIND_COUNT]
        );
    }

    #[test]
    fn array_initializer_panic_retires_only_initialized_prefix() {
        extern crate std;
        use core::sync::atomic::{AtomicUsize, Ordering};
        struct Element {
            account: Arc<MemoryAccount>,
            dropped: Arc<AtomicUsize>,
        }
        impl Drop for Element {
            fn drop(&mut self) {
                assert!(self.account.snapshot().current > 0);
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        let (account, object) = account();
        let dropped = Arc::new(AtomicUsize::new(0));
        let storage = OwnedBox::<[Element; 8]>::prepare(&object, KernelByteKind::SlabUnreclaimable)
            .unwrap()
            .try_allocate()
            .unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            storage.init_array(|index| {
                if index == 3 {
                    panic!("bounded array initialization failure");
                }
                Element {
                    account: account.clone(),
                    dropped: dropped.clone(),
                }
            })
        }));
        assert!(result.is_err());
        assert_eq!(dropped.load(Ordering::Relaxed), 3);
        assert_eq!(account.snapshot().current, 0);
        assert_eq!(
            account.snapshot().kernel_bytes,
            [0; crate::cgroup::memory::accounting::KERNEL_BYTE_KIND_COUNT]
        );
    }

    #[test]
    fn cancellation_rolls_back_shared_bytes() {
        let (account, object) = account();
        let prepared =
            OwnedBox::<[u8; 17]>::prepare(&object, KernelByteKind::SlabUnreclaimable).unwrap();
        assert_eq!(
            account.snapshot().kernel_bytes[KernelByteKind::SlabUnreclaimable as usize],
            32
        );
        drop(prepared);
        assert_eq!(account.snapshot().current, 0);
    }

    #[test]
    fn small_boxes_share_one_page_and_release_independently() {
        let (account, object) = account();
        let first = OwnedBox::prepare(&object, KernelByteKind::SlabUnreclaimable)
            .unwrap()
            .try_init([7_u8; 17])
            .unwrap();
        let second = OwnedBox::prepare(&object, KernelByteKind::SlabUnreclaimable)
            .unwrap()
            .try_init([9_u8; 17])
            .unwrap();
        assert_eq!(first[0], 7);
        assert_eq!(second[0], 9);
        assert_eq!(account.snapshot().current, 1);
        assert_eq!(
            account.snapshot().kernel_bytes[KernelByteKind::SlabUnreclaimable as usize],
            64
        );
        drop(first);
        assert_eq!(account.snapshot().current, 1);
        drop(second);
        assert_eq!(account.snapshot().current, 0);
    }

    #[test]
    fn zero_sized_box_has_no_byte_budget() {
        let (account, object) = account();
        let value = OwnedBox::prepare(&object, KernelByteKind::SlabUnreclaimable)
            .unwrap()
            .try_init(())
            .unwrap();
        assert_eq!(account.snapshot().current, 0);
        assert_eq!(
            account.snapshot().kernel_bytes,
            [0; crate::cgroup::memory::accounting::KERNEL_BYTE_KIND_COUNT]
        );
        drop(value);
    }

    #[test]
    fn admission_failure_preserves_zero_budget() {
        let root = MemoryAccount::new(1, None).unwrap();
        let account = MemoryAccount::new(2, Some(root)).unwrap();
        account.set_policy(MemoryPolicy {
            max: Some(0),
            ..MemoryPolicy::default()
        });
        let object = ObjectAccount::new(&account).unwrap();
        assert!(OwnedBox::<[u8; 17]>::prepare(&object, KernelByteKind::SlabUnreclaimable).is_err());
        assert_eq!(account.snapshot().current, 0);
    }

    #[test]
    fn value_destructor_runs_while_its_budget_is_live() {
        #[derive(Debug)]
        struct Observer(Arc<MemoryAccount>);
        impl Drop for Observer {
            fn drop(&mut self) {
                assert_ne!(self.0.snapshot().current, 0);
            }
        }
        let (account, object) = account();
        let value = OwnedBox::prepare(&object, KernelByteKind::SlabUnreclaimable)
            .unwrap()
            .try_init(Observer(account.clone()))
            .unwrap();
        drop(value);
        assert_eq!(account.snapshot().current, 0);
    }

    #[test]
    fn allocated_cancellation_does_not_drop_an_uninitialized_value() {
        struct MustNotDrop(u64);
        impl Drop for MustNotDrop {
            fn drop(&mut self) {
                panic!("uninitialized value must not be dropped: {}", self.0);
            }
        }
        let (account, object) = account();
        let allocated =
            OwnedBox::<MustNotDrop>::prepare(&object, KernelByteKind::SlabUnreclaimable)
                .unwrap()
                .try_allocate()
                .unwrap();
        assert_ne!(account.snapshot().current, 0);
        drop(allocated);
        assert_eq!(account.snapshot().current, 0);
    }

    #[test]
    fn allocated_initialization_preserves_one_receipt() {
        let (account, object) = account();
        let allocated = OwnedBox::<[u8; 17]>::prepare(&object, KernelByteKind::SlabUnreclaimable)
            .unwrap()
            .try_allocate()
            .unwrap();
        let bytes = account.snapshot().kernel_bytes;
        let value = allocated.init([11; 17]);
        assert_eq!(value[0], 11);
        assert_eq!(account.snapshot().kernel_bytes, bytes);
        drop(value);
        assert_eq!(account.snapshot().current, 0);
    }

    #[test]
    fn thin_identity_weak_does_not_retain_accounted_entity() {
        struct Identity {
            data: OwnedBox<[u8; 8193]>,
        }
        let (account, object) = account();
        let prepared = OwnedBox::<[u8; 8193]>::prepare(&object, KernelByteKind::SlabUnreclaimable)
            .unwrap()
            .try_allocate()
            .unwrap();
        let identity = Arc::try_new(Identity {
            data: prepared.init([7; 8193]),
        })
        .unwrap();
        let weak = Arc::downgrade(&identity);
        assert_eq!(identity.data[0], 7);
        let bytes = allocation_footprint(Layout::new::<[u8; 8193]>());
        assert_eq!(
            account.snapshot().kernel_bytes[KernelByteKind::KernelOther as usize],
            bytes
        );
        drop(identity);
        // This Weak still retains the real Arc header, but not the large body.
        assert!(weak.upgrade().is_none());
        assert_eq!(account.snapshot().current, 0);
        assert_eq!(
            account.snapshot().kernel_bytes[KernelByteKind::KernelOther as usize],
            0
        );
        drop(weak);
        assert_eq!(account.snapshot().current, 0);
    }
}
