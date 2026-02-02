use crate::mm::dma::{dma_alloc_pages_raw, dma_dealloc_pages_raw, DmaAllocOptions};
use core::ptr::NonNull;

pub fn dma_alloc(pages: usize) -> (usize, NonNull<u8>) {
    dma_alloc_pages_raw(pages, DmaAllocOptions::default())
}

pub unsafe fn dma_dealloc(paddr: usize, vaddr: NonNull<u8>, pages: usize) -> i32 {
    dma_dealloc_pages_raw(paddr, vaddr, pages)
}
