use crate::{exception::HardwareIrqNumber, int_like};

int_like!(ApicId, u32);

#[derive(Debug)]
pub(super) struct HardwareIrqConfig {
    pub apic_id: ApicId,
    pub vector: HardwareIrqNumber,
}
