//! A unique byte ring; physical backing dies before its kernel receipt.
use crate::{
    cgroup::memory::{
        accounting::KernelByteKind,
        object::{ObjectAccount, ObjectCharge},
    },
    mm::allocation::MemoryAllocationError,
};
use alloc::{sync::Arc, vec::Vec};
use core::{
    alloc::Layout,
    ops::{Deref, DerefMut},
};
use system_error::SystemError;

#[derive(Debug, Default)]
pub(crate) struct PipeBuffer {
    bytes: Vec<u8>,
    _charge: Option<ObjectCharge>,
}
impl PipeBuffer {
    pub(crate) fn try_new(
        size: usize,
        actor: &Arc<ObjectAccount>,
    ) -> Result<Self, MemoryAllocationError> {
        let layout = Layout::array::<u8>(size).map_err(|_| SystemError::EOVERFLOW)?;
        let charge = actor.try_charge_allocation(KernelByteKind::SlabUnreclaimable, layout)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(size)
            .map_err(|_| SystemError::ENOMEM)?;
        bytes.resize(size, 0);
        Ok(Self {
            bytes,
            _charge: Some(charge),
        })
    }
}
impl Deref for PipeBuffer {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.bytes
    }
}
impl DerefMut for PipeBuffer {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.bytes
    }
}
