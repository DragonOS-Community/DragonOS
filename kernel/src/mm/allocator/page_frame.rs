use core::{
    intrinsics::unlikely,
    ops::{Add, AddAssign, Mul, Sub, SubAssign},
};

use crate::{
    arch::{mm::LockedFrameAllocator, MMArch},
    mm::{MemoryManagementArch, PhysAddr, VirtAddr},
};

/// @brief 物理页帧的表示
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PhysPageFrame {
    /// 物理页页号
    number: usize,
}

#[allow(dead_code)]
impl PhysPageFrame {
    pub fn new(paddr: PhysAddr) -> Self {
        return Self {
            number: paddr.data() >> MMArch::PAGE_SHIFT,
        };
    }

    /// 从物理页号创建PhysPageFrame结构体
    pub fn from_ppn(ppn: usize) -> Self {
        return Self { number: ppn };
    }

    /// 获取当前页对应的物理页号
    pub fn ppn(&self) -> usize {
        return self.number;
    }

    /// @brief 获取当前页对应的物理地址
    pub fn phys_address(&self) -> PhysAddr {
        return PhysAddr::new(self.number * MMArch::PAGE_SIZE);
    }

    pub fn next_by(&self, n: usize) -> Self {
        return Self {
            number: self.number + n,
        };
    }

    pub fn next(&self) -> Self {
        return self.next_by(1);
    }

    /// 构造物理页帧的迭代器，范围为[start, end)
    pub fn iter_range(start: Self, end: Self) -> PhysPageFrameIter {
        return PhysPageFrameIter::new(start, end);
    }
}

/// @brief 物理页帧的迭代器
#[derive(Debug)]
pub struct PhysPageFrameIter {
    current: PhysPageFrame,
    /// 结束的物理页帧（不包含）
    end: PhysPageFrame,
}

impl PhysPageFrameIter {
    pub fn new(start: PhysPageFrame, end: PhysPageFrame) -> Self {
        return Self {
            current: start,
            end,
        };
    }
}

impl Iterator for PhysPageFrameIter {
    type Item = PhysPageFrame;

    fn next(&mut self) -> Option<Self::Item> {
        if unlikely(self.current == self.end) {
            return None;
        }
        let current: PhysPageFrame = self.current;
        self.current = self.current.next_by(1);
        return Some(current);
    }
}

/// 虚拟页帧的表示
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct VirtPageFrame {
    /// 虚拟页页号
    number: usize,
}

impl VirtPageFrame {
    pub fn new(vaddr: VirtAddr) -> Self {
        return Self {
            number: vaddr.data() / MMArch::PAGE_SIZE,
        };
    }

    /// 从虚拟页号创建PhysPageFrame结构体
    #[allow(dead_code)]
    pub fn from_vpn(vpn: usize) -> Self {
        return Self { number: vpn };
    }

    /// 获取当前虚拟页对应的虚拟地址
    pub fn virt_address(&self) -> VirtAddr {
        return VirtAddr::new(self.number * MMArch::PAGE_SIZE);
    }

    pub fn next_by(&self, n: usize) -> Self {
        return Self {
            number: self.number + n,
        };
    }

    pub fn next(&self) -> Self {
        return self.next_by(1);
    }

    /// 构造虚拟页帧的迭代器，范围为[start, end)
    pub fn iter_range(start: Self, end: Self) -> VirtPageFrameIter {
        return VirtPageFrameIter {
            current: start,
            end,
        };
    }

    pub fn add(&self, n: PageFrameCount) -> Self {
        return Self {
            number: self.number + n.data(),
        };
    }
}

/// 虚拟页帧的迭代器
#[derive(Debug)]
pub struct VirtPageFrameIter {
    current: VirtPageFrame,
    /// 结束的虚拟页帧(不包含)
    end: VirtPageFrame,
}

impl VirtPageFrameIter {
    /// @brief 构造虚拟页帧的迭代器，范围为[start, end)
    pub fn new(start: VirtPageFrame, end: VirtPageFrame) -> Self {
        return Self {
            current: start,
            end,
        };
    }
}

impl Iterator for VirtPageFrameIter {
    type Item = VirtPageFrame;

    fn next(&mut self) -> Option<Self::Item> {
        if unlikely(self.current == self.end) {
            return None;
        }
        let current: VirtPageFrame = self.current;
        self.current = self.current.next_by(1);
        return Some(current);
    }
}

/// 页帧使用的数量
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct PageFrameCount(usize);

impl PageFrameCount {
    pub const ONE: PageFrameCount = PageFrameCount(1);

    // @brief 初始化PageFrameCount
    pub const fn new(count: usize) -> Self {
        return Self(count);
    }
    // @brief 获取页帧数量
    pub fn data(&self) -> usize {
        return self.0;
    }

    /// 计算这一段页帧占用的字节数
    pub fn bytes(&self) -> usize {
        return self.0 * MMArch::PAGE_SIZE;
    }

    /// 将字节数转换为页帧数量
    ///
    /// 如果字节数不是页帧大小的整数倍，则返回None. 否则返回页帧数量
    pub fn from_bytes(bytes: usize) -> Option<Self> {
        if bytes & MMArch::PAGE_OFFSET_MASK != 0 {
            return None;
        } else {
            return Some(Self(bytes / MMArch::PAGE_SIZE));
        }
    }

    #[inline(always)]
    pub fn next_power_of_two(&self) -> Self {
        Self::new(self.0.next_power_of_two())
    }
}

impl Add for PageFrameCount {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        return Self(self.0 + rhs.0);
    }
}

impl AddAssign for PageFrameCount {
    fn add_assign(&mut self, rhs: Self) {
        self.0 += rhs.0;
    }
}

impl Sub for PageFrameCount {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        return Self(self.0 - rhs.0);
    }
}

impl SubAssign for PageFrameCount {
    fn sub_assign(&mut self, rhs: Self) {
        self.0 -= rhs.0;
    }
}

impl Mul for PageFrameCount {
    type Output = Self;

    fn mul(self, rhs: Self) -> Self::Output {
        return Self(self.0 * rhs.0);
    }
}

impl Add<usize> for PageFrameCount {
    type Output = Self;

    fn add(self, rhs: usize) -> Self::Output {
        return Self(self.0 + rhs);
    }
}

impl AddAssign<usize> for PageFrameCount {
    fn add_assign(&mut self, rhs: usize) {
        self.0 += rhs;
    }
}

impl Sub<usize> for PageFrameCount {
    type Output = Self;

    fn sub(self, rhs: usize) -> Self::Output {
        return Self(self.0 - rhs);
    }
}

impl SubAssign<usize> for PageFrameCount {
    fn sub_assign(&mut self, rhs: usize) {
        self.0 -= rhs;
    }
}

impl Mul<usize> for PageFrameCount {
    type Output = Self;

    fn mul(self, rhs: usize) -> Self::Output {
        return Self(self.0 * rhs);
    }
}

// 页帧使用情况
#[derive(Debug)]
pub struct PageFrameUsage {
    used: PageFrameCount,
    total: PageFrameCount,
}

#[allow(dead_code)]
impl PageFrameUsage {
    /// @brief:  初始化FrameUsage
    /// @param PageFrameCount used 已使用的页帧数量
    /// @param PageFrameCount total 总的页帧数量
    pub fn new(used: PageFrameCount, total: PageFrameCount) -> Self {
        return Self { used, total };
    }
    // @brief 获取已使用的页帧数量
    pub fn used(&self) -> PageFrameCount {
        return self.used;
    }
    // @brief 获取空闲的页帧数量
    pub fn free(&self) -> PageFrameCount {
        return self.total - self.used;
    }
    // @brief 获取总的页帧数量
    pub fn total(&self) -> PageFrameCount {
        return self.total;
    }
}

/// 能够分配页帧的分配器需要实现的trait
pub trait FrameAllocator {
    // @brief 分配count个页帧
    unsafe fn allocate(&mut self, count: PageFrameCount) -> Option<(PhysAddr, PageFrameCount)>;

    // @brief 通过地址释放count个页帧
    unsafe fn free(&mut self, address: PhysAddr, count: PageFrameCount);
    // @brief 分配一个页帧
    unsafe fn allocate_one(&mut self) -> Option<PhysAddr> {
        return self.allocate(PageFrameCount::new(1)).map(|(addr, _)| addr);
    }
    // @brief 通过地址释放一个页帧
    unsafe fn free_one(&mut self, address: PhysAddr) {
        return self.free(address, PageFrameCount::new(1));
    }
    // @brief 获取页帧使用情况
    unsafe fn usage(&self) -> PageFrameUsage;
}

/// @brief 通过一个 &mut T 的引用来对一个实现了 FrameAllocator trait 的类型进行调用，使代码更加灵活
impl<T: FrameAllocator> FrameAllocator for &mut T {
    unsafe fn allocate(&mut self, count: PageFrameCount) -> Option<(PhysAddr, PageFrameCount)> {
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

/// @brief 从全局的页帧分配器中分配连续count个页帧
///
/// @param count 请求分配的页帧数量
pub unsafe fn allocate_page_frames(count: PageFrameCount) -> Option<(PhysAddr, PageFrameCount)> {
    let frame = unsafe { LockedFrameAllocator.allocate(count)? };
    return Some(frame);
}

/// @brief 向全局页帧分配器释放连续count个页帧
///
/// @param frame 要释放的第一个页帧
/// @param count 要释放的页帧数量 (必须是2的n次幂)
pub unsafe fn deallocate_page_frames(frame: PhysPageFrame, count: PageFrameCount) {
    unsafe {
        LockedFrameAllocator.free(frame.phys_address(), count);
    };
}
