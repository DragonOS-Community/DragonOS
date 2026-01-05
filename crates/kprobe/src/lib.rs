#![cfg_attr(target_arch = "riscv64", feature(riscv_ext_intrinsics))]
#![no_std]
extern crate alloc;

mod arch;

pub use arch::*;
