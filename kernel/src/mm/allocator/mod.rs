pub mod buddy;
pub mod bump;
mod footprint;
pub(crate) mod heap;
pub mod kernel_allocator;
pub mod page_frame;
pub(crate) mod physical;
pub(crate) mod reserve;
pub mod slab;
pub mod watermark;

#[cfg(all(
    feature = "mm_selftest",
    any(target_arch = "x86_64", target_arch = "riscv64")
))]
pub(crate) mod selftest;
