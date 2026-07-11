#![no_std]

extern crate alloc;

use alloc::{boxed::Box, vec};
use core::{mem::ManuallyDrop, slice};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferState {
    Uninitialized,
    Submitted,
    Completed,
    Reusable,
    Invalid,
    ResetRetired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferError {
    InvalidLength,
    InvalidState,
    NotDirectMapped,
    DmaIdentityChanged,
    UsedLengthOverflow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionLimits {
    pub max_count: usize,
    pub max_capacity_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionSnapshot {
    pub count: usize,
    pub capacity_bytes: usize,
}

/// Pure accounting state for bounding completed device storage retained by consumers.
///
/// Synchronization and wakeups deliberately live in the transport using this type. A successful
/// reservation must be paired with exactly one `release` unless ownership is moved to another
/// object that retains that obligation.
#[derive(Debug)]
pub struct RetentionCredits {
    limits: RetentionLimits,
    used: RetentionSnapshot,
}

impl RetentionCredits {
    pub const fn new(limits: RetentionLimits) -> Self {
        Self {
            limits,
            used: RetentionSnapshot {
                count: 0,
                capacity_bytes: 0,
            },
        }
    }

    pub fn try_reserve(&mut self, capacity: usize) -> bool {
        if capacity == 0 {
            return false;
        }
        let Some(count) = self.used.count.checked_add(1) else {
            return false;
        };
        let Some(capacity_bytes) = self.used.capacity_bytes.checked_add(capacity) else {
            return false;
        };
        if count > self.limits.max_count || capacity_bytes > self.limits.max_capacity_bytes {
            return false;
        }
        self.used = RetentionSnapshot {
            count,
            capacity_bytes,
        };
        true
    }

    pub fn release(&mut self, capacity: usize) -> bool {
        if self.used.count == 0 || capacity == 0 || capacity > self.used.capacity_bytes {
            return false;
        }
        self.used.count -= 1;
        self.used.capacity_bytes -= capacity;
        true
    }

    /// Replace the capacity charged to one live reservation without changing its count.
    pub fn resize(&mut self, old_capacity: usize, new_capacity: usize) -> bool {
        if self.used.count == 0 || old_capacity == 0 || new_capacity == 0 {
            return false;
        }
        let Some(without_old) = self.used.capacity_bytes.checked_sub(old_capacity) else {
            return false;
        };
        let Some(capacity_bytes) = without_old.checked_add(new_capacity) else {
            return false;
        };
        if capacity_bytes > self.limits.max_capacity_bytes {
            return false;
        }
        self.used.capacity_bytes = capacity_bytes;
        true
    }

    pub const fn snapshot(&self) -> RetentionSnapshot {
        self.used
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DmaIdentity {
    vaddr: usize,
    paddr: usize,
    len: usize,
}

/// A fixed-address, device-writable buffer whose safe readable extent is established only by a
/// validated device completion.
///
/// The backing bytes are initialized once at allocation because virtio-drivers currently accepts
/// `&mut [u8]`, not `MaybeUninit<u8>`. Reuse makes the bytes logically uninitialized: no safe byte
/// accessor exists until `complete_after_pop` accepts the used length.
pub struct DeviceOutputBuffer {
    backing: ManuallyDrop<Box<[u8]>>,
    allocation_identity: DmaIdentity,
    writable_len: usize,
    initialized_len: usize,
    submitted_identity: Option<DmaIdentity>,
    state: BufferState,
}

impl core::fmt::Debug for DeviceOutputBuffer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DeviceOutputBuffer")
            .field("allocation_len", &self.allocation_len())
            .field("writable_len", &self.writable_len)
            .field("initialized_len", &self.initialized_len)
            .field("state", &self.state)
            .finish()
    }
}

impl DeviceOutputBuffer {
    pub fn new<F>(len: usize, mut virt_to_phys: F) -> Result<Self, BufferError>
    where
        F: FnMut(usize) -> Option<usize>,
    {
        if len == 0 {
            return Err(BufferError::InvalidLength);
        }
        let backing = vec![0u8; len].into_boxed_slice();
        let vaddr = backing.as_ptr() as usize;
        let paddr = virt_to_phys(vaddr).ok_or(BufferError::NotDirectMapped)?;
        Ok(Self {
            backing: ManuallyDrop::new(backing),
            allocation_identity: DmaIdentity { vaddr, paddr, len },
            writable_len: len,
            initialized_len: 0,
            submitted_identity: None,
            state: BufferState::Uninitialized,
        })
    }

    pub fn allocation_len(&self) -> usize {
        self.allocation_identity.len
    }

    pub fn writable_len(&self) -> usize {
        self.writable_len
    }

    pub fn state(&self) -> BufferState {
        self.state
    }

    pub fn prepare<F>(&mut self, writable_len: usize, virt_to_phys: F) -> Result<(), BufferError>
    where
        F: FnMut(usize) -> Option<usize>,
    {
        if self.state != BufferState::Reusable {
            return Err(BufferError::InvalidState);
        }
        if writable_len == 0 || writable_len > self.allocation_len() {
            return Err(BufferError::InvalidLength);
        }
        self.verify_allocation_identity(virt_to_phys)?;
        self.writable_len = writable_len;
        self.initialized_len = 0;
        self.submitted_identity = None;
        self.state = BufferState::Uninitialized;
        Ok(())
    }

    /// Returns the exact device-writable range for queue submission.
    ///
    /// # Safety
    ///
    /// The caller must use the slice only to create the device descriptor, must not inspect its
    /// contents, and must call `mark_submitted` immediately if queue acceptance succeeds.
    pub unsafe fn submission_dma_slice<F>(
        &mut self,
        virt_to_phys: F,
    ) -> Result<&mut [u8], BufferError>
    where
        F: FnMut(usize) -> Option<usize>,
    {
        if self.state != BufferState::Uninitialized {
            return Err(BufferError::InvalidState);
        }
        self.verify_allocation_identity(virt_to_phys)?;
        Ok(unsafe {
            slice::from_raw_parts_mut(self.allocation_identity.vaddr as *mut u8, self.writable_len)
        })
    }

    /// Records queue ownership after a successful add. This operation is intentionally infallible
    /// so no error path can occur between descriptor acceptance and the Submitted state.
    pub fn mark_submitted(&mut self) {
        assert_eq!(self.state, BufferState::Uninitialized);
        self.submitted_identity = Some(DmaIdentity {
            vaddr: self.allocation_identity.vaddr,
            paddr: self.allocation_identity.paddr,
            len: self.writable_len,
        });
        self.state = BufferState::Submitted;
    }

    /// Returns the original DMA range solely for pop/detach identity matching.
    ///
    /// # Safety
    ///
    /// The caller must use the slice only with the exact queue token returned for this submission,
    /// must not inspect it, and must keep this owner alive if queue retirement fails.
    pub unsafe fn retirement_dma_slice<F>(
        &mut self,
        virt_to_phys: F,
    ) -> Result<&mut [u8], BufferError>
    where
        F: FnMut(usize) -> Option<usize>,
    {
        if self.state != BufferState::Submitted {
            return Err(BufferError::InvalidState);
        }
        self.verify_allocation_identity(virt_to_phys)?;
        let identity = self.submitted_identity.ok_or(BufferError::InvalidState)?;
        let current = DmaIdentity {
            vaddr: self.allocation_identity.vaddr,
            paddr: self.allocation_identity.paddr,
            len: self.writable_len,
        };
        if identity != current {
            return Err(BufferError::DmaIdentityChanged);
        }
        Ok(unsafe { slice::from_raw_parts_mut(identity.vaddr as *mut u8, identity.len) })
    }

    /// Retires device ownership and marks exactly `used_len` bytes safely readable.
    ///
    /// # Safety
    ///
    /// The caller must have successfully removed the descriptor associated with this buffer from
    /// the used ring. A `Submitted` state alone does not prove that the device has stopped writing.
    pub unsafe fn complete_after_pop(&mut self, used_len: usize) -> Result<(), BufferError> {
        if self.state != BufferState::Submitted {
            return Err(BufferError::InvalidState);
        }
        self.submitted_identity = None;
        if used_len > self.writable_len {
            self.initialized_len = 0;
            self.state = BufferState::Invalid;
            return Err(BufferError::UsedLengthOverflow);
        }
        self.initialized_len = used_len;
        self.state = BufferState::Completed;
        Ok(())
    }

    pub fn initialized_prefix(&self) -> Result<&[u8], BufferError> {
        if self.state != BufferState::Completed {
            return Err(BufferError::InvalidState);
        }
        Ok(unsafe {
            slice::from_raw_parts(
                self.allocation_identity.vaddr as *const u8,
                self.initialized_len,
            )
        })
    }

    /// Retires a submitted buffer after device reset and descriptor detachment both succeeded.
    ///
    /// # Safety
    ///
    /// The caller must have reset the device and successfully detached the exact descriptor
    /// associated with this buffer, so the device can no longer access the backing allocation.
    pub unsafe fn retire_after_reset_and_detach(&mut self) -> Result<(), BufferError> {
        if self.state != BufferState::Submitted {
            return Err(BufferError::InvalidState);
        }
        self.submitted_identity = None;
        self.initialized_len = 0;
        self.state = BufferState::ResetRetired;
        Ok(())
    }

    pub fn recycle(&mut self) -> Result<(), BufferError> {
        if !matches!(
            self.state,
            BufferState::Uninitialized | BufferState::Completed
        ) {
            return Err(BufferError::InvalidState);
        }
        self.initialized_len = 0;
        self.submitted_identity = None;
        self.state = BufferState::Reusable;
        Ok(())
    }

    fn verify_allocation_identity<F>(&self, mut virt_to_phys: F) -> Result<(), BufferError>
    where
        F: FnMut(usize) -> Option<usize>,
    {
        let paddr =
            virt_to_phys(self.allocation_identity.vaddr).ok_or(BufferError::NotDirectMapped)?;
        if paddr != self.allocation_identity.paddr
            || self.backing.as_ptr() as usize != self.allocation_identity.vaddr
            || self.backing.len() != self.allocation_identity.len
        {
            return Err(BufferError::DmaIdentityChanged);
        }
        Ok(())
    }
}

impl Drop for DeviceOutputBuffer {
    fn drop(&mut self) {
        if self.state == BufferState::Submitted {
            // Fail safe: a live descriptor may still reference this allocation. The bridge's
            // normal reset-timeout path quarantines its whole context; this is the last-resort
            // safety net for unexpected owner drops.
            return;
        }
        unsafe { ManuallyDrop::drop(&mut self.backing) };
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BufferError, BufferState, DeviceOutputBuffer, RetentionCredits, RetentionLimits,
        RetentionSnapshot,
    };

    fn direct(vaddr: usize) -> Option<usize> {
        Some(vaddr.wrapping_sub(0x1000))
    }

    #[test]
    fn exposes_only_validated_completion_prefix() {
        let mut buf = DeviceOutputBuffer::new(128, direct).unwrap();
        assert_eq!(buf.initialized_prefix(), Err(BufferError::InvalidState));
        let ptr = unsafe { buf.submission_dma_slice(direct).unwrap().as_ptr() };
        buf.mark_submitted();
        assert_eq!(buf.initialized_prefix(), Err(BufferError::InvalidState));
        let retired_ptr = unsafe { buf.retirement_dma_slice(direct).unwrap().as_ptr() };
        assert_eq!(ptr, retired_ptr);
        unsafe { buf.complete_after_pop(17).unwrap() };
        assert_eq!(buf.initialized_prefix().unwrap().len(), 17);
    }

    #[test]
    fn reuse_keeps_allocation_and_hides_old_prefix() {
        let mut buf = DeviceOutputBuffer::new(256, direct).unwrap();
        let ptr = unsafe { buf.submission_dma_slice(direct).unwrap().as_ptr() };
        buf.mark_submitted();
        unsafe { buf.complete_after_pop(128).unwrap() };
        buf.recycle().unwrap();
        buf.prepare(192, direct).unwrap();
        assert_eq!(buf.allocation_len(), 256);
        assert_eq!(buf.writable_len(), 192);
        assert_eq!(buf.initialized_prefix(), Err(BufferError::InvalidState));
        assert_eq!(
            unsafe { buf.submission_dma_slice(direct).unwrap().as_ptr() },
            ptr
        );
    }

    #[test]
    fn oversized_completion_is_never_exposed_or_recycled() {
        let mut buf = DeviceOutputBuffer::new(64, direct).unwrap();
        unsafe { buf.submission_dma_slice(direct).unwrap() };
        buf.mark_submitted();
        assert_eq!(
            unsafe { buf.complete_after_pop(65) },
            Err(BufferError::UsedLengthOverflow)
        );
        assert_eq!(buf.state(), BufferState::Invalid);
        assert_eq!(buf.initialized_prefix(), Err(BufferError::InvalidState));
        assert_eq!(buf.recycle(), Err(BufferError::InvalidState));
    }

    #[test]
    fn identity_change_blocks_retirement_view() {
        let mut buf = DeviceOutputBuffer::new(64, direct).unwrap();
        unsafe { buf.submission_dma_slice(direct).unwrap() };
        buf.mark_submitted();
        assert_eq!(
            unsafe { buf.retirement_dma_slice(|v| Some(v.wrapping_sub(0x2000))) },
            Err(BufferError::DmaIdentityChanged)
        );
        assert_eq!(buf.state(), BufferState::Submitted);
    }

    #[test]
    fn reset_retirement_never_exposes_bytes() {
        let mut buf = DeviceOutputBuffer::new(64, direct).unwrap();
        unsafe { buf.submission_dma_slice(direct).unwrap() };
        buf.mark_submitted();
        unsafe { buf.retirement_dma_slice(direct).unwrap() };
        unsafe { buf.retire_after_reset_and_detach().unwrap() };
        assert_eq!(buf.state(), BufferState::ResetRetired);
        assert_eq!(buf.initialized_prefix(), Err(BufferError::InvalidState));
        assert_eq!(buf.recycle(), Err(BufferError::InvalidState));
    }

    #[test]
    fn invalid_lengths_and_states_are_rejected() {
        assert!(matches!(
            DeviceOutputBuffer::new(0, direct),
            Err(BufferError::InvalidLength)
        ));
        let mut buf = DeviceOutputBuffer::new(64, direct).unwrap();
        assert_eq!(buf.prepare(32, direct), Err(BufferError::InvalidState));
        buf.recycle().unwrap();
        assert_eq!(buf.prepare(0, direct), Err(BufferError::InvalidLength));
        assert_eq!(buf.prepare(65, direct), Err(BufferError::InvalidLength));
    }

    #[test]
    fn retention_credits_enforce_count_and_capacity() {
        let mut credits = RetentionCredits::new(RetentionLimits {
            max_count: 2,
            max_capacity_bytes: 96,
        });
        assert!(credits.try_reserve(64));
        assert!(!credits.try_reserve(64));
        assert!(credits.try_reserve(32));
        assert!(!credits.try_reserve(1));
        assert_eq!(
            credits.snapshot(),
            RetentionSnapshot {
                count: 2,
                capacity_bytes: 96,
            }
        );
        assert!(credits.release(64));
        assert!(credits.try_reserve(48));
        assert_eq!(credits.snapshot().capacity_bytes, 80);
    }

    #[test]
    fn retention_credits_reject_invalid_release_and_overflow() {
        let mut credits = RetentionCredits::new(RetentionLimits {
            max_count: usize::MAX,
            max_capacity_bytes: usize::MAX,
        });
        assert!(!credits.release(1));
        assert!(!credits.try_reserve(0));
        assert!(credits.try_reserve(usize::MAX));
        assert!(!credits.try_reserve(1));
        assert!(!credits.release(0));
        assert!(credits.release(usize::MAX));
        assert_eq!(
            credits.snapshot(),
            RetentionSnapshot {
                count: 0,
                capacity_bytes: 0
            }
        );
    }

    #[test]
    fn retention_credits_resize_preserves_count_and_enforces_capacity() {
        let mut credits = RetentionCredits::new(RetentionLimits {
            max_count: 2,
            max_capacity_bytes: 96,
        });
        assert!(credits.try_reserve(64));
        assert!(credits.resize(64, 80));
        assert_eq!(
            credits.snapshot(),
            RetentionSnapshot {
                count: 1,
                capacity_bytes: 80,
            }
        );
        assert!(!credits.resize(80, 97));
        assert!(!credits.resize(0, 1));
        assert!(credits.resize(80, 48));
        assert_eq!(credits.snapshot().count, 1);
        assert_eq!(credits.snapshot().capacity_bytes, 48);
        assert!(credits.release(48));
    }
}
