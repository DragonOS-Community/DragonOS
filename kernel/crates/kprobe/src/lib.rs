#![cfg_attr(target_arch = "riscv64", feature(stdsimd))]
#![no_std]
#![no_main]
extern crate alloc;

mod arch;

pub use arch::*;
