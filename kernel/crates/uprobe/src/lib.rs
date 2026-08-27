#![no_std]
//! Userspace breakpoint probe (uprobe) support -- architecture-independent core and x86_64
//! instruction analysis.
//!
//! Key difference from kprobe: the probed instruction lives in **user address space**, so
//! single-stepping must use XOL (eXecute Out of Line, a copy executed in a user-mode slot page);
//! rip cannot point at a kernel buffer (supervisor-only + NX at CPL=3). Hence this crate:
//! - only stores the original instruction copy and the XOL slot offset (**no** kernel-mode
//!   "single-step address"); instruction analysis reuses yaxpeax-x86 (a kprobe dependency);
//! - keeps the architecture-independent hit-callback interfaces; MM/exception/perf lifecycles
//!   are owned by the corresponding kernel modules.

/// Maximum size in bytes of a user-mode instruction copy. x86-64 instructions are at most 15
/// bytes; 16 bytes also serves as the XOL slot width for fixed-size copying and alignment.
pub const UPROBE_INSN_COPY_SIZE: usize = 16;

pub mod arch;

pub use arch::*;
