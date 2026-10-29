//! Temporary primary-write storage. The receipt covers actual allocator
//! capacity, not requested user length or the stack-resident Vec header.

use alloc::{sync::Arc, vec::Vec};
use core::mem;

use crate::mm::allocator::heap::{self, AllocationCharge};
use crate::{
    cgroup::memory::{
        accounting::KernelByteKind,
        object::{ObjectAccount, ObjectCharge},
    },
    mm::allocation::MemoryAllocationError,
};

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
        let (mut candidate, charge) =
            heap::object_vec::<u8>(creator, KernelByteKind::SlabUnreclaimable, needed)?;
        candidate.extend_from_slice(&self.storage);
        let old_storage = mem::replace(&mut self.storage, candidate);
        let old_charge = self.charge.replace(charge);
        heap::drop_vec(old_storage, old_charge.map(AllocationCharge::Object));
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
        heap::drop_vec(
            mem::take(&mut self.storage),
            self.charge.take().map(AllocationCharge::Object),
        );
    }
}
