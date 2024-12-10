use system_error::SystemError;

use super::dragonstub::early_dragonstub_init;

#[derive(Debug)]
#[repr(u64)]
pub(super) enum BootProtocol {
    DragonStub = 1,
}

pub(super) fn early_boot_init(protocol: BootProtocol) -> Result<(), SystemError> {
    match protocol {
        BootProtocol::DragonStub => early_dragonstub_init(),
    }
}
