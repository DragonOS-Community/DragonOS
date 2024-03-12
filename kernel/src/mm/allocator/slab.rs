//! 当前slab分配器暂时不使用，等待后续完善后合并主线
#![allow(dead_code)]

use core::alloc::Layout;

// 定义Slab，用来存放空闲块
pub struct Slab {
    block_size: usize,
    free_block_list: FreeBlockList,
}

impl Slab {
    /// @brief: 初始化一个slab
    /// @param {usize} start_addr
    /// @param {usize} slab_size
    /// @param {usize} block_size
    pub unsafe fn new(start_addr: usize, slab_size: usize, block_size: usize) -> Slab {
        let blocks_num = slab_size / block_size;
        return Slab {
            block_size,
            free_block_list: FreeBlockList::new(start_addr, block_size, blocks_num),
        };
    }

    /// @brief: 获取slab中可用的block数
    pub fn used_blocks(&self) -> usize {
        return self.free_block_list.len();
    }

    /// @brief: 扩大free_block_list
    /// @param {*} mut
    /// @param {usize} start_addr
    /// @param {usize} slab_size
    pub fn grow(&mut self, start_addr: usize, slab_size: usize) {
        let num_of_blocks = slab_size / self.block_size;
        let mut block_list =
            unsafe { FreeBlockList::new(start_addr, self.block_size, num_of_blocks) };
        // 将新链表接到原链表的后面
        while let Some(block) = block_list.pop() {
            self.free_block_list.push(block);
        }
    }
    /// @brief: 从slab中分配一个block
    /// @return 分配的内存地址
    pub fn allocate(&mut self, _layout: Layout) -> Option<*mut u8> {
        match self.free_block_list.pop() {
            Some(block) => return Some(block.addr() as *mut u8),
            None => return None,
        }
    }
    /// @brief: 将block归还给slab
    pub fn free(&mut self, ptr: *mut u8) {
        let ptr = ptr as *mut FreeBlock;
        unsafe {
            self.free_block_list.push(&mut *ptr);
        }
    }
}
/// slab中的空闲块
struct FreeBlockList {
    len: usize,
    head: Option<&'static mut FreeBlock>,
}

impl FreeBlockList {
    unsafe fn new(start_addr: usize, block_size: usize, num_of_blocks: usize) -> FreeBlockList {
        let mut new_list = FreeBlockList::new_empty();
        for i in (0..num_of_blocks).rev() {
            // 从后往前分配，避免内存碎片
            let new_block = (start_addr + i * block_size) as *mut FreeBlock;
            new_list.push(&mut *new_block);
        }
        return new_list;
    }

    fn new_empty() -> FreeBlockList {
        return FreeBlockList { len: 0, head: None };
    }

    fn len(&self) -> usize {
        return self.len;
    }

    /// @brief: 将空闲块从链表中弹出
    fn pop(&mut self) -> Option<&'static mut FreeBlock> {
        // 从链表中弹出一个空闲块
        let block = self.head.take().map(|node| {
            self.head = node.next.take();
            self.len -= 1;
            node
        });
        return block;
    }

    /// @brief: 将空闲块压入链表
    fn push(&mut self, free_block: &'static mut FreeBlock) {
        free_block.next = self.head.take();
        self.len += 1;
        self.head = Some(free_block);
    }

    fn is_empty(&self) -> bool {
        return self.head.is_none();
    }
}

impl Drop for FreeBlockList {
    fn drop(&mut self) {
        while let Some(_) = self.pop() {}
    }
}

struct FreeBlock {
    next: Option<&'static mut FreeBlock>,
}

impl FreeBlock {
    /// @brief: 获取FreeBlock的地址
    /// @return {*}
    fn addr(&self) -> usize {
        return self as *const _ as usize;
    }
}
