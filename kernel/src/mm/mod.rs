use core::ptr;

use crate::include::bindings::bindings::PAGE_OFFSET;

pub mod allocator;
pub mod gfp;
pub mod mmio_buddy;
pub mod page;

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

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum PageTableKind {
    /// 用户可访问的页表
    User,
    /// 内核页表
    Kernel,
}

/// 物理内存地址
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct PhysAddr(usize);

impl PhysAddr {
    #[inline(always)]
    pub const fn new(address: usize) -> Self {
        Self(address)
    }

    /// @brief 获取物理地址的值
    #[inline(always)]
    pub fn data(&self) -> usize {
        self.0
    }

    /// @brief 将物理地址加上一个偏移量
    #[inline(always)]
    pub fn add(self, offset: usize) -> Self {
        Self(self.0 + offset)
    }
}

/// 虚拟内存地址
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct VirtAddr(usize);

impl VirtAddr {
    #[inline(always)]
    pub const fn new(address: usize) -> Self {
        return Self(address);
    }

    /// @brief 获取虚拟地址的值
    #[inline(always)]
    pub fn data(&self) -> usize {
        return self.0;
    }

    /// @brief 将虚拟地址加上一个偏移量
    #[inline(always)]
    pub fn add(self, offset: usize) -> Self {
        return Self(self.0 + offset);
    }

    /// @brief 判断虚拟地址的类型
    #[inline(always)]
    pub fn kind(&self) -> PageTableKind {
        if (self.0 as isize) < 0 {
            return PageTableKind::Kernel;
        } else {
            return PageTableKind::User;
        }
    }
}

/// @brief 物理内存区域
#[derive(Clone, Copy, Debug)]
pub struct PhysMemoryArea {
    /// 物理基地址
    pub base: PhysAddr,
    /// 该区域的物理内存大小
    pub size: usize,
}

pub trait MemoryManagementArch: Clone + Copy {
    /// 页面大小的shift（假如页面4K，那么这个值就是12,因为2^12=4096）
    const PAGE_SHIFT: usize;
    /// 每个页表的页表项数目。（以2^n次幂来表示）假如有512个页表项，那么这个值就是9
    const PAGE_ENTRY_SHIFT: usize;
    /// 页表层级数量
    const PAGE_LEVELS: usize;

    /// 页表项的有效位的index（假如页表项的第0-51位有效，那么这个值就是52）
    const ENTRY_ADDRESS_SHIFT: usize;
    /// 页面的页表项的默认值
    const ENTRY_FLAG_DEFAULT_PAGE: usize;
    /// 页表的页表项的默认值
    const ENTRY_FLAG_DEFAULT_TABLE: usize;
    /// 页表项的present位被置位之后的值
    const ENTRY_FLAG_PRESENT: usize;
    /// 页表项为read only时的值
    const ENTRY_FLAG_READONLY: usize;
    /// 页表项为可读写状态的值
    const ENTRY_FLAG_READWRITE: usize;
    /// 页面项标记页面为user page的值
    const ENTRY_FLAG_USER: usize;
    /// 页面项标记页面为write through的值
    const ENTRY_FLAG_WRITE_THROUGH: usize;
    /// 页面项标记页面为cache disable的值
    const ENTRY_FLAG_CACHE_DISABLE: usize;
    /// 标记当前页面不可执行的标志位（Execute disable）（也就是说，不能从这段内存里面获取处理器指令）
    const ENTRY_FLAG_NO_EXEC: usize;
    /// 标记当前页面可执行的标志位（Execute enable）
    const ENTRY_FLAG_EXEC: usize;

    /// 虚拟地址与物理地址的偏移量
    const PHYS_OFFSET: usize;

    /// 每个页面的大小
    const PAGE_SIZE: usize = 1 << Self::PAGE_SHIFT;
    /// 通过这个mask，获取地址的页内偏移量
    const PAGE_OFFSET_MASK: usize = Self::PAGE_SIZE - 1;
    /// 页表项的地址、数据部分的shift。
    /// 打个比方，如果这个值为52,那么意味着页表项的[0, 52)位，用于表示地址以及其他的标志位
    const PAGE_ADDRESS_SHIFT: usize = Self::PAGE_LEVELS * Self::PAGE_ENTRY_SHIFT + Self::PAGE_SHIFT;
    /// 最大的虚拟地址（对于不同的架构，由于上述PAGE_ADDRESS_SHIFT可能包括了reserved bits, 事实上能表示的虚拟地址应该比这个值要小）
    const PAGE_ADDRESS_SIZE: usize = 1 << Self::PAGE_ADDRESS_SHIFT;
    /// 页表项的值与这个常量进行与运算，得到的结果是所填写的物理地址
    const PAGE_ADDRESS_MASK: usize = Self::PAGE_ADDRESS_SIZE - Self::PAGE_SIZE;
    /// 每个页表项的大小
    const PAGE_ENTRY_SIZE: usize = 1 << (Self::PAGE_SHIFT - Self::PAGE_ENTRY_SHIFT);
    /// 每个页表的页表项数目
    const PAGE_ENTRY_NUM: usize = 1 << Self::PAGE_ENTRY_SHIFT;
    /// 该字段用于根据虚拟地址，获取该虚拟地址在对应的页表中是第几个页表项
    const PAGE_ENTRY_MASK: usize = Self::PAGE_ENTRY_NUM - 1;

    const PAGE_NEGATIVE_MASK: usize = !((Self::PAGE_ADDRESS_SIZE) - 1);

    const ENTRY_ADDRESS_SIZE: usize = 1 << Self::ENTRY_ADDRESS_SHIFT;
    /// 该mask用于获取页表项中地址字段
    const ENTRY_ADDRESS_MASK: usize = Self::ENTRY_ADDRESS_SIZE - Self::PAGE_SIZE;
    /// 这个mask用于获取页表项中的flags
    const ENTRY_FLAGS_MASK: usize = !Self::ENTRY_ADDRESS_MASK;

    /// @brief 用于初始化内存管理模块与架构相关的信息。
    /// 该函数应调用其他模块的接口，生成内存区域结构体，提供给BumpAllocator使用
    unsafe fn init() -> &'static [PhysMemoryArea];

    /// @brief 读取指定虚拟地址的值，并假设它是类型T的指针
    #[inline(always)]
    unsafe fn read<T>(address: VirtAddr) -> T {
        return ptr::read(address.data() as *const T);
    }

    /// @brief 将value写入到指定的虚拟地址
    #[inline(always)]
    unsafe fn write<T>(address: VirtAddr, value: T) {
        ptr::write(address.data() as *mut T, value);
    }
    /// @brief 刷新TLB中，关于指定虚拟地址的条目
    unsafe fn invalidate_page(address: VirtAddr);

    /// @brief 刷新TLB中，所有的条目
    unsafe fn invalidate_all();

    /// @brief 获取顶级页表的物理地址
    unsafe fn table(table_kind: PageTableKind) -> PhysAddr;

    /// @brief 设置顶级页表的物理地址到处理器中
    unsafe fn set_table(table_kind: PageTableKind, table: PhysAddr);

    /// @brief 将物理地址转换为虚拟地址.
    ///
    /// @param phys 物理地址
    ///
    /// @return 转换后的虚拟地址。如果转换失败，返回None
    #[inline(always)]
    unsafe fn phys_2_virt(phys: PhysAddr) -> Option<VirtAddr> {
        if let Some(vaddr) = phys.data().checked_add(Self::PHYS_OFFSET) {
            return Some(VirtAddr::new(vaddr));
        } else {
            return None;
        }
    }

    /// @brief 判断指定的虚拟地址是否正确（符合规范）
    fn virt_is_valid(virt: VirtAddr) -> bool;
}
