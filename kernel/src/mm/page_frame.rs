/*
 * @Auther: Kong
 * @Date: 2023-03-27 11:57:07
 * @FilePath: /DragonOS/kernel/src/mm/page_frame.rs
 * @Description: 页帧分配器
 */
use crate::mm::PhysAddr;

#[derive(Clone, Copy, Debug)]
#[repr(transparent)]
// 页帧使用的数量
pub struct PageFrameCount(usize);

impl PageFrameCount {
    // @brief 初始化PageFrameCount
    pub fn new(count: usize) -> Self {
        return Self(count);
    }
    // @brief 获取页帧数量
    pub fn data(&self) -> usize {
        return self.0;
    }
}
// 页帧使用情况
#[derive(Debug)]
pub struct PageFrameUsage {
    used: PageFrameCount,
    total: PageFrameCount,
}

impl PageFrameUsage {
    /**
     * @description: 初始化FrameUsage
     * @param {PageFrameCount} used 已使用的页帧数量
     * @param {PageFrameCount} total 总的页帧数量
     * @return {*}
     */    
    pub fn new(used: PageFrameCount, total: PageFrameCount) -> Self {
        return Self { used, total };
    }
    // @brief 获取已使用的页帧数量
    pub fn used(&self) -> PageFrameCount {
        return self.used;
    }
    // @brief 获取空闲的页帧数量
    pub fn free(&self) -> PageFrameCount {
        return PageFrameCount(self.total.0 - self.used.0);
    }
    // @brief 获取总的页帧数量
    pub fn total(&self) -> PageFrameCount {
        return self.total;
    }
}
// 能够分配页帧的分配器需要实现的trait
pub trait FrameAllocator {
    // @brief 分配count个页帧
    unsafe fn allocate(&mut self, count: PageFrameCount) -> Option<PhysAddr>;

    // @brief 通过地址释放count个页帧
    unsafe fn free(&mut self, address: PhysAddr, count: PageFrameCount);
    // @brief 分配一个页帧
    unsafe fn allocate_one(&mut self) -> Option<PhysAddr> {
        return self.allocate(PageFrameCount::new(1));
    }
    // @brief 通过地址释放一个页帧
    unsafe fn free_one(&mut self, address: PhysAddr) {
        return self.free(address, PageFrameCount::new(1));
    }
    // @brief 获取页帧使用情况
    unsafe fn usage(&self) -> PageFrameUsage;
}
// @brief 通过一个 &mut T 的引用来对一个实现了 FrameAllocator trait 的类型进行调用，使代码更加灵活
impl<T> FrameAllocator for &mut T
where
    T: FrameAllocator,
{
    unsafe fn allocate(&mut self, count: PageFrameCount) -> Option<PhysAddr> {
        return T::allocate(self, count);
    }
    unsafe fn free(&mut self, address: PhysAddr, count: PageFrameCount) {
        return T::free(self, address, count);
    }
    unsafe fn allocate_one(&mut self) -> Option<PhysAddr> {
        return T::allocate_one(self);
    }
    unsafe fn free_one(&mut self, address: PhysAddr) {
        return T::free_one(self, address);
    }
    unsafe fn usage(&self) -> PageFrameUsage {
        return T::usage(self);
    }
}
