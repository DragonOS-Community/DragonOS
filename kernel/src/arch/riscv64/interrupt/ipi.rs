use sbi_rt::HartMask;

use crate::{
    arch::mm::RiscV64MMArch,
    exception::ipi::{IpiKind, IpiTarget},
    smp::core::smp_get_processor_id,
};

#[inline(always)]
pub fn send_ipi(kind: IpiKind, target: IpiTarget) {
    let mask = Into::into(target);
    match kind {
        IpiKind::KickCpu => todo!(),
        IpiKind::FlushTLB => RiscV64MMArch::remote_invalidate_all_with_mask(mask).ok(),
        IpiKind::SpecVector(_) => todo!(),
    };
}

impl Into<HartMask> for IpiTarget {
    fn into(self) -> HartMask {
        match self {
            IpiTarget::All => HartMask::from_mask_base(usize::MAX, 0),
            IpiTarget::Other => {
                let data = usize::MAX & (!(1 << smp_get_processor_id().data()));
                let mask = HartMask::from_mask_base(data, 0);
                mask
            }
            IpiTarget::Specified(cpu_id) => {
                let mask = Into::into(cpu_id);
                mask
            }
            IpiTarget::Current => {
                let mask = Into::into(smp_get_processor_id());
                mask
            }
        }
    }
}
