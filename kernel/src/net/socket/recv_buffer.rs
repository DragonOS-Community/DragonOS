//! Lazy CPU receive storage. Protocol owners select and consume data; this
//! owner only admits storage, with no socket locks held during recovery.
use crate::{filesystem::vfs::file::File, mm::allocation::MemoryAllocationError};
use system_error::SystemError;

#[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
use crate::mm::kernel_buffer::KernelBuffer as ReceiveStorage;
#[cfg(not(any(target_arch = "x86_64", target_arch = "riscv64")))]
type ReceiveStorage = alloc::vec::Vec<u8>;

pub(crate) enum ReceiveAttempt<T> {
    Ready(T),
    /// Nothing was consumed. Drop every queue/transport lock before preparing.
    NeedBuffer(usize),
}

#[derive(Default)]
pub(crate) struct RecvBuffer {
    storage: Option<ReceiveStorage>,
}

impl RecvBuffer {
    pub(crate) fn capacity(&self) -> usize {
        self.storage.as_ref().map_or(0, |buffer| buffer.len())
    }

    /// Call only outside socket locks. Recovery retries allocation alone,
    /// never message selection, consumption, or user copying.
    pub(crate) fn prepare(&mut self, required: usize) -> Result<(), SystemError> {
        File::recover_io(|| self.try_prepare_with(required, allocate))
    }

    fn try_prepare_with(
        &mut self,
        required: usize,
        allocate: impl FnOnce(usize) -> Result<ReceiveStorage, MemoryAllocationError>,
    ) -> Result<(), MemoryAllocationError> {
        if self.capacity() >= required {
            return Ok(());
        }
        // No borrowed slice survives growth. Avoid charging old and new
        // storage together; failure leaves an empty, reusable owner.
        drop(self.storage.take());
        self.storage = Some(allocate(required)?);
        Ok(())
    }

    /// Only bytes completely overwritten by the protocol may be exposed.
    pub(crate) fn bytes(&self, len: usize) -> &[u8] {
        match &self.storage {
            Some(storage) => &storage[..len],
            None => &[][..len],
        }
    }

    pub(crate) fn bytes_mut(&mut self, len: usize) -> &mut [u8] {
        match &mut self.storage {
            Some(storage) => &mut storage[..len],
            None => &mut [][..len],
        }
    }
}

fn allocate(required: usize) -> Result<ReceiveStorage, MemoryAllocationError> {
    #[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
    {
        ReceiveStorage::try_new_zeroed(required)
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "riscv64")))]
    {
        let mut storage = ReceiveStorage::new();
        storage
            .try_reserve_exact(required)
            .map_err(|_| SystemError::ENOMEM)?;
        storage.resize(required, 0);
        Ok(storage)
    }
}

#[cfg(all(
    feature = "mm_selftest",
    any(target_arch = "x86_64", target_arch = "riscv64")
))]
mod selftest;
#[cfg(all(
    feature = "mm_selftest",
    any(target_arch = "x86_64", target_arch = "riscv64")
))]
pub(crate) use selftest::run as run_selftest;
