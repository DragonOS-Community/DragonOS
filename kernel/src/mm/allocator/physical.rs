//! Explicit direct-mapped, physically contiguous storage for CPU hardware state.
//!
//! Allocation uses the local slab/buddy allocator. The global deallocator must
//! preserve its direct-map branch so ordinary Box destruction remains symmetric.
use alloc::boxed::Box;
use core::{
    alloc::{AllocError, Layout},
    fmt,
    mem::MaybeUninit,
    ops::{Deref, DerefMut},
    ptr::NonNull,
};

use crate::{
    arch::MMArch,
    mm::{MemoryManagementArch, PhysAddr, VirtAddr},
    KERNEL_ALLOCATOR,
};

use super::kernel_allocator::LocalAlloc;

/// A hardware-visible page whose allocation and release use the same layout.
#[repr(C, align(4096))]
#[derive(Debug)]
pub(crate) struct PhysicalPage(pub(crate) [u8; 4096]);

impl Deref for PhysicalPage {
    type Target = [u8; 4096];
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl DerefMut for PhysicalPage {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

pub(crate) struct PhysicalBox<T: ?Sized>(Box<T>);

impl<T: ?Sized> fmt::Debug for PhysicalBox<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PhysicalBox")
            .field("pointer", &(&**self as *const T))
            .field("size", &core::mem::size_of_val(&**self))
            .finish()
    }
}

fn allocate(layout: Layout, zeroed: bool) -> Result<NonNull<u8>, AllocError> {
    if layout.size() == 0 {
        // Preserve the alignment of the exact layout, including aligned ZSTs.
        return Ok(unsafe { NonNull::new_unchecked(layout.align() as *mut u8) });
    }
    let raw = unsafe {
        if zeroed {
            KERNEL_ALLOCATOR.local_alloc_zeroed(layout)
        } else {
            KERNEL_ALLOCATOR.local_alloc(layout)
        }
    };
    NonNull::new(raw).ok_or(AllocError)
}

impl<T> PhysicalBox<T> {
    pub(crate) fn try_new(value: T) -> Result<Self, AllocError> {
        let mut storage = Self::try_new_uninit()?;
        storage.write(value);
        Ok(unsafe { storage.assume_init() })
    }

    pub(crate) fn try_new_uninit() -> Result<PhysicalBox<MaybeUninit<T>>, AllocError> {
        let raw = allocate(Layout::new::<T>(), false)?;
        // SAFETY: LocalAlloc and GlobalAlloc's direct branch share the exact
        // layout and ownership rules. MaybeUninit imposes no value validity.
        Ok(PhysicalBox(unsafe { Box::from_raw(raw.cast().as_ptr()) }))
    }

    pub(crate) fn try_new_zeroed() -> Result<PhysicalBox<MaybeUninit<T>>, AllocError> {
        let raw = allocate(Layout::new::<T>(), true)?;
        Ok(PhysicalBox(unsafe { Box::from_raw(raw.cast().as_ptr()) }))
    }
}

impl<T> PhysicalBox<MaybeUninit<T>> {
    /// # Safety
    /// The stored value must be fully initialized and valid for T.
    pub(crate) unsafe fn assume_init(self) -> PhysicalBox<T> {
        PhysicalBox(self.0.assume_init())
    }
}

impl<T> PhysicalBox<[T]> {
    pub(crate) fn try_new_zeroed_slice(
        len: usize,
    ) -> Result<PhysicalBox<[MaybeUninit<T>]>, AllocError> {
        let layout = Layout::array::<T>(len).map_err(|_| AllocError)?;
        let raw = allocate(layout, true)?;
        Ok(PhysicalBox(unsafe {
            Box::from_raw(core::ptr::slice_from_raw_parts_mut(
                raw.cast().as_ptr(),
                len,
            ))
        }))
    }
}

impl<T> PhysicalBox<[MaybeUninit<T>]> {
    /// # Safety
    /// Every element must be fully initialized and valid for T.
    pub(crate) unsafe fn assume_init(self) -> PhysicalBox<[T]> {
        PhysicalBox(self.0.assume_init())
    }
}

impl<T: ?Sized> PhysicalBox<T> {
    pub(crate) fn into_inner(self) -> Box<T> {
        self.0
    }

    pub(crate) fn phys_addr(&self) -> Option<PhysAddr> {
        if core::mem::size_of_val(&**self) == 0 {
            return None;
        }
        // SAFETY: only local direct allocations can construct this owner.
        unsafe {
            MMArch::virt_2_phys(VirtAddr::new(
                self.0.as_ref() as *const T as *const u8 as usize
            ))
        }
    }
}

impl<T: ?Sized> Deref for PhysicalBox<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T: ?Sized> DerefMut for PhysicalBox<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

impl<T: ?Sized> AsRef<T> for PhysicalBox<T> {
    fn as_ref(&self) -> &T {
        self
    }
}

impl<T: ?Sized> AsMut<T> for PhysicalBox<T> {
    fn as_mut(&mut self) -> &mut T {
        self
    }
}

/// Exercise the explicit contract after ordinary large Box allocations have
/// switched backends: physical storage stays direct, aligned and move-stable.
#[cfg(feature = "mm_selftest")]
pub(crate) fn check_physical_owners() -> Result<bool, system_error::SystemError> {
    #[repr(C, align(16384))]
    struct OverAligned([u8; 16384]);

    let mut aligned = unsafe {
        PhysicalBox::<OverAligned>::try_new_zeroed()
            .map_err(|_| system_error::SystemError::ENOMEM)?
            .assume_init()
    };
    let address = (*aligned).0.as_ptr() as usize;
    let Some(physical) = aligned.phys_addr() else {
        return Ok(false);
    };
    if !address.is_multiple_of(16384)
        || !physical.data().is_multiple_of(16384)
        || (*aligned).0.iter().any(|byte| *byte != 0)
    {
        return Ok(false);
    }
    (*aligned).0[16383] = 0x5a;
    let retained = alloc::sync::Arc::new(aligned);
    let moved = retained.clone();
    drop(retained);
    if (**moved).0.as_ptr() as usize != address || (**moved).0[16383] != 0x5a {
        return Ok(false);
    }
    drop(moved);

    let bytes = unsafe {
        PhysicalBox::<[u8]>::try_new_zeroed_slice(3 * MMArch::PAGE_SIZE)
            .map_err(|_| system_error::SystemError::ENOMEM)?
            .assume_init()
    };
    let zst = unsafe {
        PhysicalBox::<()>::try_new_zeroed()
            .map_err(|_| system_error::SystemError::ENOMEM)?
            .assume_init()
    };
    Ok(bytes.phys_addr().is_some()
        && bytes.iter().all(|byte| *byte == 0)
        && zst.phys_addr().is_none())
}
