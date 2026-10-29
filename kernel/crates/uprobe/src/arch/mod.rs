//! Architecture-independent trait definitions and dispatch for uprobe.
//!
//! - [`ProbeArgs`]: architecture-independent view of processor registers/trap frames (for callbacks).
//! - [`CallBackFunc`]: the event callback (typically the entry point of an eBPF program).

use ::core::any::Any;

#[cfg(target_arch = "x86_64")]
mod x86;

#[cfg(target_arch = "x86_64")]
pub use x86::*;

/// Architecture-independent view of processor registers/trap frames (for callbacks).
///
/// Keeps the same signature as kprobe's `ProbeArgs` to reuse the same TrapFrame
/// adaptation pattern; however, uprobe is a standalone crate that does not depend on the
/// kprobe crate (low coupling), so it is defined separately.
pub trait ProbeArgs: Send {
    /// Allows the user to convert to a specific architecture's TrapFrame.
    fn as_any(&self) -> &dyn Any;
    /// The instruction address that triggered the breakpoint exception (#BP); for uprobe
    /// this is probe_vaddr.
    fn break_address(&self) -> usize;
    /// The instruction address that triggered the single-step exception (#DB); the next
    /// instruction after the original one in the XOL slot.
    fn debug_address(&self) -> usize;
}

/// The event callback (typically the entry point of an eBPF program).
///
/// Same signature as kprobe's `CallBackFunc`. Uses `Arc` so the same callback instance can
/// be shared across multiple per-mm probe sites.
pub trait CallBackFunc: Send + Sync {
    fn call(&self, trap_frame: &dyn ProbeArgs);
}
