#![allow(dead_code)]
use super::{ecall::ecall2, SbiError};

/// System reset extension ID
pub const EXTENSION_ID: usize = 0x53525354;

/// The type of reset to perform
#[derive(Debug, Clone, Copy)]
pub enum ResetType {
    /// Shutdown the system
    Shutdown,
    /// Power off all hardware and perform a cold boot
    ColdReboot,
    /// Reset processors and some hardware
    WarmReboot,
    /// Platform specific reset type. The variant value is a value within the
    /// range `0x00000000..=0x0FFFFFFF`. A value outside of that range will be
    /// clamped to the maximum possible valid value for this reset type.
    PlatformSpecific(u32),
}

impl ResetType {
    fn to_u32(self) -> u32 {
        match self {
            ResetType::Shutdown => 0,
            ResetType::ColdReboot => 1,
            ResetType::WarmReboot => 2,
            ResetType::PlatformSpecific(n) => n.min(0x0FFFFFFF) + 0xF0000000,
        }
    }
}

/// The reason for performing the reset
#[derive(Debug, Clone, Copy)]
pub enum ResetReason {
    /// No reason for reset
    NoReason,
    /// System failure
    SystemFailure,
    /// SBI implementation specific reset reason. The variant value is a value
    /// within the range `0x00000000..=0x0FFFFFFF`. A value outside of that
    /// range will be clamped to the maximum possible valid value for this reset
    /// reason type.
    SbiSpecific(u32),
    /// Platform specific reset reason. The variant value is a value within the
    /// range `0x00000000..=0x0FFFFFFF`. A value outside of that range will be
    /// clamped to the maximum possible valid value for this reset reason type.
    PlatformSpecific(u32),
}

impl ResetReason {
    fn to_u32(self) -> u32 {
        match self {
            ResetReason::NoReason => 0,
            ResetReason::SystemFailure => 1,
            ResetReason::SbiSpecific(n) => n.min(0x0FFFFFFF) + 0xE0000000,
            ResetReason::PlatformSpecific(n) => n.min(0x0FFFFFFF) + 0xF0000000,
        }
    }
}

/// Attempt to reset the system in the provided method, with a reason for the
/// reset.
///
/// ### Possible errors
///
/// [`SbiError::NotSupported`]: The [`ResetType`] is valid but not implemented.
///
/// [`SbiError::Failed`]: The system reset request failed for an unknown reason.
pub fn system_reset(
    kind: ResetType,
    reason: ResetReason,
) -> Result<core::convert::Infallible, SbiError> {
    match unsafe {
        ecall2(
            kind.to_u32() as usize,
            reason.to_u32() as usize,
            EXTENSION_ID,
            0,
        )
    } {
        Ok(_) => unreachable!("SBI returned `Ok` after a system reset call"),
        Err(e) => Err(e),
    }
}
