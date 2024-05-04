use alloc::sync::Arc;
use system_error::SystemError;

use crate::virt::vm::kvm_host::{
    mem::{KvmMemoryChangeMode, LockedKvmMemSlot},
    Vm,
};

pub struct KvmArchMemorySlot {}

impl Vm {
    pub fn arch_prepare_memory_region(
        &self,
        old: Option<&Arc<LockedKvmMemSlot>>,
        new: Option<&Arc<LockedKvmMemSlot>>,
        change: KvmMemoryChangeMode,
    ) -> Result<(), SystemError> {
        // todo
        Ok(())
    }
}
