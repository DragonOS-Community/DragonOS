//! A slab allocator implementation for objects less than a page-size (4 KiB or 2MiB).
//!
//! # Overview
//!
//! The organization is as follows:
//!
//!  * A `ZoneAllocator` manages many `SCAllocator` and can
//!    satisfy requests for different allocation sizes.
//!  * A `SCAllocator` allocates objects of exactly one size.
//!    It stores the objects and meta-data in one or multiple `AllocablePage` objects.
//!  * A trait `AllocablePage` that defines the page-type from which we allocate objects.
//!
//! Lastly, it provides two default `AllocablePage` implementations `ObjectPage` and `LargeObjectPage`:
//!  * A `ObjectPage` that is 4 KiB in size and contains allocated objects and associated meta-data.
//!  * A `LargeObjectPage` that is 2 MiB in size and contains allocated objects and associated meta-data.
//!
//!
//! # Implementing GlobalAlloc
//! See the [global alloc](https://github.com/gz/rust-slabmalloc/tree/master/examples/global_alloc.rs) example.
#![allow(unused_features)]
#![cfg_attr(test, feature(test, c_void_variant))]
#![no_std]
#![crate_name = "slabmalloc"]
#![crate_type = "lib"]
#![feature(maybe_uninit_as_bytes)]
#![deny(clippy::all)]
#![allow(clippy::needless_return)]
extern crate alloc;

mod pages;
mod sc;
mod zone;

pub use pages::*;
pub use sc::*;
pub use zone::*;

#[cfg(test)]
#[macro_use]
extern crate std;
#[cfg(test)]
extern crate test;

#[cfg(test)]
mod tests;

use core::alloc::Layout;
use core::fmt;
use core::mem;
use core::ptr::{self, NonNull};

use log::trace;

/// How many bytes in the page are used by allocator meta-data.
const OBJECT_PAGE_METADATA_OVERHEAD: usize = 80;

/// How many bytes a [`ObjectPage`] is.
const OBJECT_PAGE_SIZE: usize = 4096;

type VAddr = usize;

/// Error that can be returned for `allocation` and `deallocation` requests.
#[derive(Debug)]
pub enum AllocationError {
    /// Can't satisfy the allocation request for Layout because the allocator
    /// does not have enough memory (you may be able to `refill` it).
    OutOfMemory,
    /// Allocator can't deal with the provided size of the Layout.
    InvalidLayout,
}

/// Allocator trait to be implemented by users of slabmalloc to provide memory to slabmalloc.
///
/// # Safety
/// Needs to adhere to safety requirements of a rust allocator (see GlobalAlloc et. al.).
pub unsafe trait Allocator<'a> {
    fn allocate(&mut self, layout: Layout) -> Result<NonNull<u8>, AllocationError>;
    /// # Safety
    /// The caller must ensure that the memory is valid and that the layout is correct.
    unsafe fn deallocate(
        &mut self,
        ptr: NonNull<u8>,
        layout: Layout,
        slab_callback: &'static dyn CallBack,
    ) -> Result<(), AllocationError>;
    /// Refill the allocator with a [`ObjectPage`].
    ///
    /// # Safety
    /// TBD (this API needs to change anyways, likely new page should be a raw pointer)
    unsafe fn refill(
        &mut self,
        layout: Layout,
        new_page: &'a mut ObjectPage<'a>,
    ) -> Result<(), AllocationError>;
}

/// 将slab_page归还Buddy的回调函数
pub trait CallBack: Send + Sync {
    /// # Safety
    /// The caller must ensure that the memory is valid and that the size is correct.
    unsafe fn free_slab_page(&self, _: *mut u8, _: usize) {}
}
