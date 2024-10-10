#![no_std]
#![feature(core_intrinsics)]
#![allow(incomplete_features)] // for const generics
#![feature(generic_const_exprs)]
#![allow(clippy::needless_return)]

#[macro_use]
extern crate alloc;

mod alloc_bitmap;
mod bitmap_core;
mod static_bitmap;
pub mod traits;
pub use alloc_bitmap::AllocBitmap;
pub use static_bitmap::StaticBitmap;
