use sbi_rt::HartMask;

use crate::{
    arch::mm::RiscV64MMArch,
    exception::ipi::{IpiKind, IpiTarget},
    smp::core::smp_get_processor_id,
};

/// IPI delivery on RISC-V:
///
/// - `FlushTLB` broadcasts TLB invalidation synchronously via SBI `remote_sfence_vma`;
///   when the call returns, all target CPUs have completed invalidation. No per-CPU CSD
///   ack protocol is needed (unlike x86_64).
/// - Therefore `crate::mm::tlb::flush_tlb_multi` on RISC-V goes directly here without
///   requiring the initiator to poll for ack before returning to userspace.
///
/// Other direct-send paths for `IpiKind::FlushTLB` (e.g. the old `InactiveFlusher` /
/// `PageFlushAll`) have been removed on x86_64; the RISC-V implementation is kept here
/// to facilitate future integration with the new `mm::tlb::flush_tlb_multi` flow.
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
