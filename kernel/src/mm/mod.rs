use crate::include::bindings::bindings::PAGE_OFFSET;

pub mod allocator;
pub mod gfp;
pub mod mmio_buddy;

/// @brief 将内核空间的虚拟地址转换为物理地址
#[inline(always)]
pub fn virt_2_phys(addr: usize) -> usize {
    addr - PAGE_OFFSET as usize
}

/// @brief 将物理地址转换为内核空间的虚拟地址
#[inline(always)]
pub fn phys_2_virt(addr: usize) -> usize {
    addr + PAGE_OFFSET as usize
}
