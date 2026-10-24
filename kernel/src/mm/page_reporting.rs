//! Exclusive ownership of free memory offered to a host for content discard.
//! Call finish only after used-ring completion, or before a failed submission
//! became visible to the device. Dropping a pending token never frees memory.
use crate::mm::PhysAddr;

#[must_use = "the isolated block must remain owned until device completion"]
#[derive(Debug)]
pub struct ReportingBlock {
    address: PhysAddr,
    pub(crate) order: u8,
}

impl ReportingBlock {
    pub(crate) fn new(address: PhysAddr, order: u8) -> Self {
        Self { address, order }
    }
    pub fn address(&self) -> PhysAddr {
        self.address
    }
    pub fn bytes(&self) -> usize {
        1usize << self.order
    }
}

pub fn try_take() -> Option<ReportingBlock> {
    #[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
    {
        crate::arch::mm::LockedFrameAllocator::with_buddy(|buddy| buddy.take_reporting_block())
            .flatten()
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "riscv64")))]
    {
        None
    }
}

pub fn finish(block: ReportingBlock, reported: bool) {
    #[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
    {
        crate::arch::mm::LockedFrameAllocator::with_buddy(|buddy| {
            buddy.finish_reporting_block(block, reported)
        })
        .expect("reporting allocator disappeared");
        // Publish after releasing the allocator lock; waiters recheck their
        // own admission predicate rather than treating a wake as recovery.
        crate::mm::memory_notify::request_oom_progress();
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "riscv64")))]
    {
        let _ = (block, reported);
        unreachable!("reporting is unsupported on this architecture");
    }
}

#[cfg(all(
    feature = "mm_selftest",
    any(target_arch = "x86_64", target_arch = "riscv64")
))]
pub(crate) fn selftest_report() -> alloc::string::String {
    crate::mm::allocator::buddy::reporting_selftest_report()
}
