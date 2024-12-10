use alloc::sync::Arc;
use log::warn;
use system_error::SystemError;

use crate::virt::vm::kvm_host::{
    mem::{KvmMemoryChangeMode, LockedKvmMemSlot},
    Vm,
};

#[allow(dead_code)]
pub struct KvmArchMemorySlot {}

impl Vm {
    pub fn arch_prepare_memory_region(
        &self,
        _old: Option<&Arc<LockedKvmMemSlot>>,
        _new: Option<&Arc<LockedKvmMemSlot>>,
        _change: KvmMemoryChangeMode,
    ) -> Result<(), SystemError> {
        // todo
        warn!("arch_prepare_memory_region TODO");
        Ok(())
    }
}
