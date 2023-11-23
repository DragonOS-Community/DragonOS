use crate::exception::ipi::{IpiKind, IpiTarget};

#[inline(always)]
pub fn send_ipi(kind: IpiKind, target: IpiTarget) {
    unimplemented!("RiscV64 send_ipi")
}
