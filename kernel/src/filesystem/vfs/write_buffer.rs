//! Temporary primary-write storage. The receipt covers actual allocator
//! capacity, not requested user length or the stack-resident Vec header.

use alloc::{sync::Arc, vec::Vec};
use core::{alloc::Layout, mem};

use crate::{
    cgroup::memory::{
        accounting::KernelByteKind,
        object::{ObjectAccount, ObjectCharge},
    },
    mm::allocation::MemoryAllocationError,
};
use system_error::SystemError;

pub(super) struct WriteBuffer {
    storage: Vec<u8>,
    charge: Option<ObjectCharge>,
}

impl WriteBuffer {
    pub(super) fn new() -> Self {
        Self {
            storage: Vec::new(),
            charge: None,
        }
    }

    pub(super) fn try_reserve(
        &mut self,
        creator: &Arc<ObjectAccount>,
        needed: usize,
    ) -> Result<(), MemoryAllocationError> {
        if self.storage.capacity() >= needed {
            return Ok(());
        }
        let layout = Layout::array::<u8>(needed).map_err(|_| SystemError::ENOMEM)?;
        let charge = creator.try_charge_allocation(KernelByteKind::SlabUnreclaimable, layout)?;
        let mut candidate = Vec::new();
        candidate
            .try_reserve_exact(needed)
            .map_err(|_| SystemError::ENOMEM)?;
        candidate.extend_from_slice(&self.storage);
        let old_storage = mem::replace(&mut self.storage, candidate);
        let old_charge = self.charge.replace(charge);
        drop(old_storage);
        drop(old_charge);
        Ok(())
    }

    pub(super) fn bytes(&self) -> &[u8] {
        &self.storage
    }
    pub(super) fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.storage
    }
    pub(super) fn resize(&mut self, len: usize) {
        assert!(len <= self.storage.capacity());
        self.storage.resize(len, 0);
    }
    pub(super) fn extend(&mut self, bytes: &[u8]) {
        assert!(bytes.len() <= self.storage.capacity() - self.storage.len());
        self.storage.extend_from_slice(bytes);
    }
}

impl Drop for WriteBuffer {
    fn drop(&mut self) {
        drop(mem::take(&mut self.storage));
        drop(self.charge.take());
    }
}
