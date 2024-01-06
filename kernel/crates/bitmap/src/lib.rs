#![no_std]
#![feature(core_intrinsics)]
#![allow(incomplete_features)] // for const generics
#![feature(generic_const_exprs)]

mod bitmap_core;
mod static_bitmap;
pub mod traits;
pub use static_bitmap::StaticBitmap;
