use core::sync::atomic::AtomicU32;

mod c_adapter;

int_like!(ProcessorId, AtomicProcessorId, u32, AtomicU32);

impl ProcessorId {
    pub const INVALID: ProcessorId = ProcessorId::new(u32::MAX);
}
