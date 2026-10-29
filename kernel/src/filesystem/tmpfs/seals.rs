//! Seals for shmem-backed files.  Ordinary tmpfs files start permanently
//! sealed against adding more seals; memfd opts into mutable seals at creation.

use crate::mm::VmFlags;
use system_error::SystemError;

pub const F_SEAL_SEAL: u32 = 0x0001;
pub const F_SEAL_SHRINK: u32 = 0x0002;
pub const F_SEAL_GROW: u32 = 0x0004;
pub const F_SEAL_WRITE: u32 = 0x0008;
pub const F_SEAL_FUTURE_WRITE: u32 = 0x0010;
pub const F_SEAL_EXEC: u32 = 0x0020;
pub const F_ALL_SEALS: u32 =
    F_SEAL_SEAL | F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE | F_SEAL_FUTURE_WRITE | F_SEAL_EXEC;

#[derive(Debug)]
pub(super) struct MemfdSeals {
    pub(super) bits: u32,
    /// Writer reservations made before MAP_FIXED can discard an old mapping.
    pub(super) pending_writable_maps: usize,
    /// Published MAP_SHARED mappings with VM_MAYWRITE, including clones/splits.
    pub(super) writable_maps: usize,
    /// Remote writers that have pinned a file page but not finished copying.
    pub(super) remote_writers: usize,
}

impl MemfdSeals {
    pub(super) fn new(bits: u32) -> Self {
        Self {
            bits,
            pending_writable_maps: 0,
            writable_maps: 0,
            remote_writers: 0,
        }
    }

    pub(super) fn prepare_map(
        &mut self,
        mut flags: VmFlags,
    ) -> Result<(VmFlags, bool), SystemError> {
        if !flags.contains(VmFlags::VM_SHARED | VmFlags::VM_MAYWRITE) {
            return Ok((flags, false));
        }
        if self.bits & (F_SEAL_WRITE | F_SEAL_FUTURE_WRITE) != 0 {
            if flags.contains(VmFlags::VM_WRITE) {
                return Err(SystemError::EPERM);
            }
            // Linux permits new read-only shared maps, but not mprotect WRITE.
            flags.remove(VmFlags::VM_MAYWRITE);
            return Ok((flags, false));
        }
        self.pending_writable_maps += 1;
        Ok((flags, true))
    }

    pub(super) fn add(&mut self, requested: u32, executable: bool) -> Result<(), SystemError> {
        if self.bits & F_SEAL_SEAL != 0 {
            return Err(SystemError::EPERM);
        }
        // Linux checks only an explicitly requested WRITE bit before adding
        // the implicit WRITE bits associated with sealing executable content.
        if requested & F_SEAL_WRITE != 0
            && self.bits & F_SEAL_WRITE == 0
            && (self.pending_writable_maps != 0
                || self.writable_maps != 0
                || self.remote_writers != 0)
        {
            return Err(SystemError::EBUSY);
        }
        let mut added = requested;
        if requested & F_SEAL_EXEC != 0 && executable {
            added |= F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE | F_SEAL_FUTURE_WRITE;
        }
        self.bits |= added;
        Ok(())
    }

    pub(super) fn permitted_write_len(
        &self,
        offset: usize,
        len: usize,
        size: usize,
    ) -> Result<usize, SystemError> {
        if self.bits & (F_SEAL_WRITE | F_SEAL_FUTURE_WRITE) != 0 {
            return Err(SystemError::EPERM);
        }
        if self.bits & F_SEAL_GROW != 0 {
            let permitted = len.min(size.saturating_sub(offset));
            if permitted == 0 {
                return Err(SystemError::EPERM);
            }
            return Ok(permitted);
        }
        Ok(len)
    }

    pub(super) fn check_resize(&self, old: usize, new: usize) -> Result<(), SystemError> {
        if (new < old && self.bits & F_SEAL_SHRINK != 0)
            || (new > old && self.bits & F_SEAL_GROW != 0)
        {
            return Err(SystemError::EPERM);
        }
        Ok(())
    }
}
