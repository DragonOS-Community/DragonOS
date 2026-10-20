#[cfg(test)]
pub mod fake;

use crate::{Error, Result, PAGE_SIZE};
use core::{marker::PhantomData, ptr::NonNull};

/// A physical address as used for virtio.
pub type PhysAddr = usize;

/// A region of contiguous physical memory used for DMA.
#[derive(Debug)]
pub struct Dma<H: Hal> {
    paddr: usize,
    vaddr: NonNull<u8>,
    pages: usize,
    _hal: PhantomData<H>,
}

impl<H: Hal> Dma<H> {
    /// Allocates the given number of pages of physically contiguous memory to be used for DMA in
    /// the given direction.
    ///
    /// The pages will be zeroed.
    pub fn new(pages: usize, direction: BufferDirection) -> Result<Self> {
        let (paddr, vaddr) = H::try_dma_alloc(pages, direction)?;
        if paddr == 0 {
            return Err(Error::DmaError);
        }
        Ok(Self {
            paddr,
            vaddr,
            pages,
            _hal: PhantomData,
        })
    }

    /// Returns the physical address of the start of the DMA region, as seen by devices.
    pub fn paddr(&self) -> usize {
        self.paddr
    }

    /// Returns a pointer to the given offset within the DMA region.
    pub fn vaddr(&self, offset: usize) -> NonNull<u8> {
        assert!(offset < self.pages * PAGE_SIZE);
        NonNull::new((self.vaddr.as_ptr() as usize + offset) as _).unwrap()
    }

    /// Returns a pointer to the entire DMA region as a slice.
    pub fn raw_slice(&self) -> NonNull<[u8]> {
        let raw_slice =
            core::ptr::slice_from_raw_parts_mut(self.vaddr(0).as_ptr(), self.pages * PAGE_SIZE);
        NonNull::new(raw_slice).unwrap()
    }
}

impl<H: Hal> Drop for Dma<H> {
    fn drop(&mut self) {
        // Safe because the memory was previously allocated by `try_dma_alloc` in `Dma::new`, not yet
        // deallocated, and we are passing the values from then.
        let err = unsafe { H::dma_dealloc(self.paddr, self.vaddr, self.pages) };
        assert_eq!(err, 0, "failed to deallocate DMA");
    }
}

/// The interface which a particular hardware implementation must implement.
///
/// # Safety
///
/// Implementations of this trait must follow the "implementation safety" requirements documented
/// for each method. Callers must follow the safety requirements documented for the unsafe methods.
pub unsafe trait Hal {
    /// Allocates and zeroes the given number of contiguous physical pages of DMA memory for VirtIO
    /// use.
    ///
    /// Returns both the physical address which the device can use to access the memory, and a
    /// pointer to the start of it which the driver can use to access it.
    ///
    /// # Implementation safety
    ///
    /// Implementations of this method must ensure that the `NonNull<u8>` returned is a
    /// [_valid_](https://doc.rust-lang.org/std/ptr/index.html#safety) pointer, aligned to
    /// [`PAGE_SIZE`], and won't alias any other allocations or references in the program until it
    /// is deallocated by `dma_dealloc`. The pages must be zeroed.
    fn dma_alloc(pages: usize, direction: BufferDirection) -> (PhysAddr, NonNull<u8>);

    /// Fallible coherent allocation. On success the same implementation safety
    /// requirements as `dma_alloc` apply, including zeroing and unique ownership.
    /// An error transfers no memory and requires no call to `dma_dealloc`.
    ///
    /// The default preserves existing HAL implementations. Platforms with a
    /// fallible allocator should override this method instead of returning an
    /// invalid pointer from `dma_alloc`.
    fn try_dma_alloc(pages: usize, direction: BufferDirection) -> Result<(PhysAddr, NonNull<u8>)> {
        Ok(Self::dma_alloc(pages, direction))
    }

    /// Deallocates the given contiguous physical DMA memory pages.
    ///
    /// # Safety
    ///
    /// The memory must have been allocated by `dma_alloc` or `try_dma_alloc` on the same `Hal` implementation, and not
    /// yet deallocated. `pages` must be the same number passed to the allocation method originally, and both
    /// `paddr` and `vaddr` must be the values returned by that method.
    unsafe fn dma_dealloc(paddr: PhysAddr, vaddr: NonNull<u8>, pages: usize) -> i32;

    /// Converts a physical address used for MMIO to a virtual address which the driver can access.
    ///
    /// This is only used for MMIO addresses within BARs read from the device, for the PCI
    /// transport. It may check that the address range up to the given size is within the region
    /// expected for MMIO.
    ///
    /// # Implementation safety
    ///
    /// Implementations of this method must ensure that the `NonNull<u8>` returned is a
    /// [_valid_](https://doc.rust-lang.org/std/ptr/index.html#safety) pointer, and won't alias any
    /// other allocations or references in the program.
    ///
    /// # Safety
    ///
    /// The `paddr` and `size` must describe a valid MMIO region. The implementation may validate it
    /// in some way (and panic if it is invalid) but is not guaranteed to.
    unsafe fn mmio_phys_to_virt(paddr: PhysAddr, size: usize) -> NonNull<u8>;

    /// Shares the given memory range with the device, and returns the physical address that the
    /// device can use to access it.
    ///
    /// This may involve mapping the buffer into an IOMMU, giving the host permission to access the
    /// memory, or copying it to a special region where it can be accessed.
    ///
    /// # Safety
    ///
    /// The buffer must be a valid pointer to a non-empty memory range which will not be accessed by
    /// any other thread for the duration of this method call.
    unsafe fn share(buffer: NonNull<[u8]>, direction: BufferDirection) -> PhysAddr;

    /// Unshares the given memory range from the device and (if necessary) copies it back to the
    /// original buffer.
    ///
    /// # Safety
    ///
    /// The buffer must be a valid pointer to a non-empty memory range which will not be accessed by
    /// any other thread for the duration of this method call. The `paddr` must be the value
    /// previously returned by the corresponding `share` call.
    unsafe fn unshare(paddr: PhysAddr, buffer: NonNull<[u8]>, direction: BufferDirection);
}

/// The direction in which a buffer is passed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum BufferDirection {
    /// The buffer may be read or written by the driver, but only read by the device.
    DriverToDevice,
    /// The buffer may be read or written by the device, but only read by the driver.
    DeviceToDriver,
    /// The buffer may be read or written by both the device and the driver.
    Both,
}

#[cfg(test)]
mod fallible_tests {
    use super::*;
    use crate::{
        hal::fake::FakeHal,
        queue::VirtQueue,
        transport::{
            fake::{FakeTransport, State},
            DeviceType,
        },
    };
    use alloc::{sync::Arc, vec, vec::Vec};
    use core::cell::RefCell;
    use std::sync::Mutex;

    // Per-thread state keeps parallel upstream tests independent.
    std::thread_local! {
        static CALLS: RefCell<(Vec<usize>, usize, usize)> = RefCell::new((Vec::new(), 0, 0));
    }
    struct FallibleHal;
    unsafe impl Hal for FallibleHal {
        fn dma_alloc(_: usize, _: BufferDirection) -> (PhysAddr, NonNull<u8>) {
            panic!("Dma must use the fallible entry")
        }
        fn try_dma_alloc(
            pages: usize,
            direction: BufferDirection,
        ) -> Result<(PhysAddr, NonNull<u8>)> {
            CALLS.with(|state| {
                let mut state = state.borrow_mut();
                state.0.push(pages);
                if state.0.len() == state.1 {
                    return Err(Error::DmaError);
                }
                state.2 += 1;
                Ok(FakeHal::dma_alloc(pages, direction))
            })
        }
        unsafe fn dma_dealloc(paddr: PhysAddr, vaddr: NonNull<u8>, pages: usize) -> i32 {
            CALLS.with(|state| state.borrow_mut().2 -= 1);
            unsafe { FakeHal::dma_dealloc(paddr, vaddr, pages) }
        }
        unsafe fn mmio_phys_to_virt(paddr: PhysAddr, size: usize) -> NonNull<u8> {
            unsafe { FakeHal::mmio_phys_to_virt(paddr, size) }
        }
        unsafe fn share(buffer: NonNull<[u8]>, direction: BufferDirection) -> PhysAddr {
            unsafe { FakeHal::share(buffer, direction) }
        }
        unsafe fn unshare(paddr: PhysAddr, buffer: NonNull<[u8]>, direction: BufferDirection) {
            unsafe { FakeHal::unshare(paddr, buffer, direction) }
        }
    }
    fn check<const SIZE: usize>(expected: &[usize]) {
        for (fail_on, event_idx) in
            (0..=expected.len()).flat_map(|fail| [(fail, false), (fail, true)])
        {
            CALLS.with(|state| *state.borrow_mut() = (Vec::new(), fail_on, 0));
            let state = Arc::new(Mutex::new(State {
                queues: vec![Default::default()],
                ..Default::default()
            }));
            let mut transport = FakeTransport {
                device_type: DeviceType::Block,
                max_queue_size: SIZE as u32,
                device_features: 0,
                config_space: NonNull::<()>::dangling(),
                state: state.clone(),
            };
            let mut queue =
                VirtQueue::<FallibleHal, SIZE>::new(&mut transport, 0, false, event_idx);
            if fail_on == 0 {
                assert!(queue.is_ok());
                CALLS.with(|s| assert_eq!(s.borrow().0, expected));
                assert_ne!(state.lock().unwrap().queues[0].descriptors, 0);
                // Exercise both published DMA bases and the driver's virtual
                // available ring, including the EVENT_IDX tail on completion.
                let payload = [17u8, 29, 43];
                let inputs: [&[u8]; 1] = [&payload];
                let queue = queue.as_mut().unwrap();
                // SAFETY: payload stays live and unchanged through completion.
                let token = unsafe { queue.add(&inputs, &mut []) }.unwrap();
                assert_eq!(state.lock().unwrap().read_from_queue::<SIZE>(0), payload);
                // SAFETY: the fake device completed exactly the submitted buffers.
                unsafe { queue.pop_used(token, &inputs, &mut []) }.unwrap();
            } else {
                assert!(matches!(queue, Err(Error::DmaError)));
                assert_eq!(state.lock().unwrap().queues[0].descriptors, 0);
            }
            drop(queue);
            CALLS.with(|s| {
                assert_eq!(
                    s.borrow().2,
                    0,
                    "failed area must release every preceding owner"
                )
            });
        }
    }
    #[test]
    fn modern_queue_allocation_failure_is_transactional() {
        check::<16>(&[1, 1]);
        check::<256>(&[1, 1, 1]);
        check::<512>(&[2, 1, 2]);
        check::<1024>(&[4, 1, 3]);
    }
}
