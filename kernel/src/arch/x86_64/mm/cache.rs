//! CPU cache writeback for mapped persistent-memory ranges.
use core::arch::asm;
use raw_cpuid::CpuId;
use system_error::SystemError;

#[derive(Debug, Clone, Copy)]
pub(crate) struct CacheWriteback {
    line_size: usize,
}

impl CacheWriteback {
    pub(crate) fn detect() -> Option<Self> {
        let features = CpuId::new().get_feature_info()?;
        // CPUID.01H:EBX[15:8] reports the CLFLUSH line size in eight-byte units.
        let line_size = usize::from(features.cflush_cache_line_size()) * 8;
        (features.has_clflush() && line_size.is_power_of_two() && line_size <= 4096)
            .then_some(Self { line_size })
    }

    /// The entire cache-line-rounded range must remain mapped for this call.
    pub(crate) unsafe fn flush(&self, start: usize, len: usize) -> Result<(), SystemError> {
        if len == 0 {
            return Ok(());
        }
        let end = start.checked_add(len).ok_or(SystemError::EOVERFLOW)?;
        let mut line = start & !(self.line_size - 1);
        asm!("mfence", options(nostack, preserves_flags));
        loop {
            asm!("clflush [{addr}]", addr = in(reg) line, options(nostack, preserves_flags));
            if end - line <= self.line_size {
                break;
            }
            line += self.line_size;
        }
        asm!("mfence", options(nostack, preserves_flags));
        Ok(())
    }
}
