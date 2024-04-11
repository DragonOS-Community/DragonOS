use crate::mm::fault::PageFault;

pub struct RiscV64PageFault;

impl PageFault for RiscV64PageFault {
    fn vma_access_permitted(
        vma: alloc::sync::Arc<crate::mm::ucontext::LockedVMA>,
        write: bool,
        execute: bool,
        foreign: bool,
    ) -> bool {
        true
    }
}
