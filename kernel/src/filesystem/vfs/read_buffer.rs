//! Per-syscall CPU read scratch. Backing reads and user copies stay outside
//! allocation recovery, so retries cannot consume data or advance file offsets.
use crate::{filesystem::vfs::file::File, mm::allocation::MemoryAllocationError};
use system_error::SystemError;

#[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
use crate::mm::kernel_buffer::KernelBuffer as ReadStorage;
#[cfg(not(any(target_arch = "x86_64", target_arch = "riscv64")))]
type ReadStorage = alloc::vec::Vec<u8>;

pub(crate) const MAX_CHUNK: usize = 64 * 1024;

/// One bounded, lazy owner, reused across chunks and iovecs. It never retains
/// data for a caller: every returned active slice has the old Vec's zeroing
/// contract, and growth discards storage whose data has already been copied.
#[derive(Default)]
pub(crate) struct ReadScratch {
    storage: Option<ReadStorage>,
}

impl ReadScratch {
    pub(crate) fn prepare(&mut self, want: usize) -> Result<&mut [u8], SystemError> {
        if want == 0 {
            return Ok(&mut []);
        }
        File::recover_io(|| self.try_prepare_with(want, allocate))?;
        Ok(&mut self.storage.as_mut().unwrap()[..want])
    }

    fn try_prepare_with(
        &mut self,
        want: usize,
        allocate: impl FnOnce(usize) -> Result<ReadStorage, MemoryAllocationError>,
    ) -> Result<(), MemoryAllocationError> {
        if want > MAX_CHUNK {
            return Err(SystemError::EINVAL.into());
        }
        if want == 0 {
            return Ok(());
        }
        if let Some(storage) = self.storage.as_mut().filter(|buffer| buffer.len() >= want) {
            storage[..want].fill(0);
            return Ok(());
        }

        // No active slice survives this call. Release the old owner before
        // admitting a larger one; allocation failure leaves no stale storage.
        drop(self.storage.take());
        self.storage = Some(allocate(want)?);
        Ok(())
    }
}

fn allocate(want: usize) -> Result<ReadStorage, MemoryAllocationError> {
    #[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
    {
        ReadStorage::try_new_zeroed(want)
    }
    // Architectures without vmalloc keep their existing heap contract. This
    // branch does not provide KernelBuffer's typed byte accounting.
    #[cfg(not(any(target_arch = "x86_64", target_arch = "riscv64")))]
    {
        let mut storage = ReadStorage::new();
        storage
            .try_reserve_exact(want)
            .map_err(|_| SystemError::ENOMEM)?;
        storage.resize(want, 0);
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
pub(crate) use selftest::report as selftest_report;
