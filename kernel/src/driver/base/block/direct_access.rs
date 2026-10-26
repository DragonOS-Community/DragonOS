//! Owned, bounded read-only access to memory-backed block storage.
use crate::mm::{MemoryManagementArch, PhysAddr};
use alloc::sync::Arc;
use core::fmt::Debug;
use system_error::SystemError;

/// Implementations keep the mapping and its provider alive for every operation.
/// Successful reads fill the entire output; filesystem EOF is handled by callers.
///
/// # Safety
/// Returned physical ranges must describe the same memory-like device storage
/// as `read_at`, excluding allocator-managed RAM. The provider must own the
/// range and retain its backing and canonical write-back mapping until the
/// last capability reference drops. All CPU aliases must use the same memory
/// type; removal must not revoke a live capability. Bounds and address
/// arithmetic must be checked before exposing a physical range or copying.
pub unsafe trait DirectAccess: Debug + Send + Sync {
    fn len(&self) -> usize;
    fn checked_paddr(&self, offset: usize, len: usize) -> Result<PhysAddr, SystemError>;
    fn read_at(&self, offset: usize, out: &mut [u8]) -> Result<(), SystemError>;
}

/// Cloning a range shares its existing provider lease, rather than acquiring a new pin.
#[derive(Clone, Debug)]
pub struct DirectAccessRange {
    access: Arc<dyn DirectAccess>,
    offset: usize,
    len: usize,
}

impl DirectAccessRange {
    pub fn new(access: Arc<dyn DirectAccess>) -> Self {
        let len = access.len();
        Self {
            access,
            offset: 0,
            len,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    fn absolute_offset(&self, offset: usize, len: usize) -> Result<usize, SystemError> {
        if offset.checked_add(len).ok_or(SystemError::EOVERFLOW)? > self.len {
            return Err(SystemError::ERANGE);
        }
        self.offset
            .checked_add(offset)
            .ok_or(SystemError::EOVERFLOW)
    }

    pub fn subrange(&self, offset: usize, len: usize) -> Result<Self, SystemError> {
        let offset = self.absolute_offset(offset, len)?;
        Ok(Self {
            access: self.access.clone(),
            offset,
            len,
        })
    }

    pub fn checked_paddr(&self, offset: usize, len: usize) -> Result<PhysAddr, SystemError> {
        self.access
            .checked_paddr(self.absolute_offset(offset, len)?, len)
    }

    pub fn page_paddr(&self, offset: usize) -> Result<PhysAddr, SystemError> {
        let paddr = self.checked_paddr(offset, crate::arch::MMArch::PAGE_SIZE)?;
        if !paddr.check_aligned(crate::arch::MMArch::PAGE_SIZE) {
            return Err(SystemError::EINVAL);
        }
        Ok(paddr)
    }

    pub fn read_at(&self, offset: usize, out: &mut [u8]) -> Result<(), SystemError> {
        self.access
            .read_at(self.absolute_offset(offset, out.len())?, out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug)]
    struct Storage {
        bytes: alloc::vec::Vec<u8>,
        released: Arc<AtomicUsize>,
    }
    impl Drop for Storage {
        fn drop(&mut self) {
            self.released.fetch_add(1, Ordering::SeqCst);
        }
    }
    // Test-only arithmetic oracle: these synthetic addresses are never
    // published to an MM or used for physical memory access.
    unsafe impl DirectAccess for Storage {
        fn len(&self) -> usize {
            self.bytes.len()
        }
        fn checked_paddr(&self, offset: usize, len: usize) -> Result<PhysAddr, SystemError> {
            if offset.checked_add(len).ok_or(SystemError::EOVERFLOW)? > self.bytes.len() {
                return Err(SystemError::ERANGE);
            }
            Ok(PhysAddr::new(0x10000 + offset))
        }
        fn read_at(&self, offset: usize, out: &mut [u8]) -> Result<(), SystemError> {
            self.checked_paddr(offset, out.len())?;
            out.copy_from_slice(&self.bytes[offset..offset + out.len()]);
            Ok(())
        }
    }

    fn storage() -> (DirectAccessRange, Arc<AtomicUsize>) {
        let released = Arc::new(AtomicUsize::new(0));
        let owner = Arc::new(Storage {
            bytes: (0..16384).map(|i| (i % 251) as u8).collect(),
            released: released.clone(),
        });
        (DirectAccessRange::new(owner), released)
    }

    #[test]
    fn partition_subranges_cannot_escape_or_wrap() {
        let (whole, _) = storage();
        let partition = whole.subrange(4096, 8192).unwrap();
        let nested = partition.subrange(512, 1024).unwrap();
        assert_eq!(
            nested.checked_paddr(0, 1024).unwrap(),
            PhysAddr::new(0x11200)
        );
        let mut bytes = [0; 16];
        nested.read_at(7, &mut bytes).unwrap();
        assert_eq!(bytes[0], ((4096 + 512 + 7) % 251) as u8);
        assert_eq!(nested.subrange(1024, 1).unwrap_err(), SystemError::ERANGE);
        assert_eq!(
            nested.checked_paddr(usize::MAX, 2).unwrap_err(),
            SystemError::EOVERFLOW
        );
        assert!(nested.read_at(1024, &mut []).is_ok());
        assert_eq!(nested.read_at(1024, &mut bytes), Err(SystemError::ERANGE));
    }

    #[test]
    fn page_access_requires_complete_aligned_physical_page() {
        let (whole, _) = storage();
        assert_eq!(
            whole.subrange(4096, 4096).unwrap().page_paddr(0).unwrap(),
            PhysAddr::new(0x11000)
        );
        assert_eq!(
            whole.subrange(1, 4096).unwrap().page_paddr(0),
            Err(SystemError::EINVAL)
        );
        assert_eq!(
            whole.subrange(4096, 4095).unwrap().page_paddr(0),
            Err(SystemError::ERANGE)
        );
    }

    #[test]
    fn nested_ranges_retain_one_shared_owner_until_last_drop() {
        let (whole, released) = storage();
        let page = whole.subrange(4096, 4096).unwrap();
        let cloned = page.clone();
        drop(whole);
        drop(page);
        assert_eq!(released.load(Ordering::SeqCst), 0);
        cloned.read_at(4096, &mut []).unwrap();
        drop(cloned);
        assert_eq!(released.load(Ordering::SeqCst), 1);
    }
}
