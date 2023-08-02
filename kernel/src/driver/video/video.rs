// use crate::include::bindings::bindings::PAGE_PCD;
// use crate::include::bindings::bindings::PAGE_PWT;
// use crate::include::bindings::bindings::PAGE_KERNEL_PAGE;
// use crate::include::bindings::bindings::mm_map_proc_page_table;
// use crate::libs::spinlock::SpinLock;
// use crate::{kinfo, include::bindings::bindings::{get_CR3,SPECIAL_MEMOEY_MAPPING_VIRT_ADDR_BASE, FRAME_BUFFER_MAPPING_OFFSET}};

// use lazy_static::lazy_static;
// lazy_static! {
//     pub static ref FB_INFO: SpinLock<multiboot_tag_framebuffer_info_t> = SpinLock::new(LinkedList::new());
// }
// /**
//  * @brief VBE帧缓存区的地址重新映射
//  * 将帧缓存区映射到地址SPECIAL_MEMOEY_MAPPING_VIRT_ADDR_BASE处
//  */
// pub fn init_frame_buffer() {
//     kinfo!("Re-mapping VBE frame buffer...");

//     // Get the current CR3 value and convert it to uint64_t type
//     let global_CR3 = unsafe { get_CR3() } as u64;

//     // Define a struct to hold information about framebuffer
//     struct FrameBufferInfo {
//         vaddr: usize,
//         size: usize,
//     };

//     let mut video_frame_buffer_info = FrameBufferInfo {
//         vaddr: (SPECIAL_MEMOEY_MAPPING_VIRT_ADDR_BASE + FRAME_BUFFER_MAPPING_OFFSET) as usize,
//         size: __fb_info.framebuffer_size,
//     };

//     // Map the page table with specific permissions
//     mm_map_proc_page_table(
//         global_CR3, true,
//         video_frame_buffer_info.vaddr, __fb_info.framebuffer_addr,
//         video_frame_buffer_info.size,
//         PAGE_KERNEL_PAGE | PAGE_PWT | PAGE_PCD, false, true, false
//     );

//     // Flush Translation Lookaside Buffer for the CPU
//     flush_tlb();
//     kinfo!("VBE frame buffer successfully Re-mapped!");
// }
