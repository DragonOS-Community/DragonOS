use crate::include::bindings::bindings::{mm_struct, process_control_block, PAGE_OFFSET, PAGE_2M_SHIFT,PAGE_2M_MASK,PAGE_2M_SIZE, memory_management_struct, Page};

pub mod allocator;
pub mod gfp;
pub mod mmio_buddy;
pub mod syscall;

/// @brief 将内核空间的虚拟地址转换为物理地址
#[inline(always)]
pub fn virt_2_phys(addr: usize) -> usize {
    addr - PAGE_OFFSET as usize
}
/// @brief 将addr按照x的上边界对齐
#[inline(always)]
pub fn PAGE_2M_ALIGN(addr:u32)-> u32{
    (addr + PAGE_2M_SIZE - 1)& PAGE_2M_MASK as u32
}
/// @brief 将物理地址转换为内核空间的虚拟地址
#[inline(always)]
pub fn phys_2_virt(addr: usize) -> usize {
    addr + PAGE_OFFSET as usize
}
/// @brief 获取对应的页结构体
#[inline(always)]
pub fn Phy_to_2M_Page(kaddr:usize)->*mut Page{
    unsafe { memory_management_struct.pages_struct.add(kaddr >> PAGE_2M_SHIFT)}
}
// ====== 重构内存管理后，请删除18-24行 ======
//BUG pcb问题
unsafe impl Send for process_control_block {}
unsafe impl Sync for process_control_block {}

unsafe impl Send for mm_struct {}
unsafe impl Sync for mm_struct {}
