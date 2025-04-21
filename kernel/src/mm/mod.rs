use alloc::sync::Arc;
use page::EntryFlags;
use system_error::SystemError;

use crate::arch::MMArch;

use core::{
    cmp,
    fmt::Debug,
    intrinsics::unlikely,
    ops::{Add, AddAssign, Sub, SubAssign},
    ptr,
    sync::atomic::{AtomicBool, Ordering},
};

use self::{
    allocator::page_frame::{VirtPageFrame, VirtPageFrameIter},
    memblock::MemoryAreaAttr,
    page::round_up_to_page_size,
    ucontext::{AddressSpace, LockedVMA, UserMapper},
};

pub mod allocator;
pub mod early_ioremap;
pub mod fault;
pub mod init;
pub mod kernel_mapper;
pub mod madvise;
pub mod memblock;
pub mod mmio_buddy;
pub mod no_init;
pub mod page;
pub mod percpu;
pub mod syscall;
pub mod ucontext;

/// 内核INIT进程的用户地址空间结构体（仅在process_init中初始化）
static mut __IDLE_PROCESS_ADDRESS_SPACE: Option<Arc<AddressSpace>> = None;

bitflags! {
    /// Virtual memory flags
    #[allow(clippy::bad_bit_mask)]
    pub struct VmFlags:usize{
        const VM_NONE = 0x00000000;

        const VM_READ = 0x00000001;
        const VM_WRITE = 0x00000002;
        const VM_EXEC = 0x00000004;
        const VM_SHARED = 0x00000008;

        const VM_MAYREAD = 0x00000010;
        const VM_MAYWRITE = 0x00000020;
        const VM_MAYEXEC = 0x00000040;
        const VM_MAYSHARE = 0x00000080;

        const VM_GROWSDOWN = 0x00000100;
        const VM_UFFD_MISSING = 0x00000200;
        const VM_PFNMAP = 0x00000400;
        const VM_UFFD_WP = 0x00001000;

        const VM_LOCKED = 0x00002000;
        const VM_IO = 0x00004000;

        const VM_SEQ_READ = 0x00008000;
        const VM_RAND_READ = 0x00010000;

        const VM_DONTCOPY = 0x00020000;
        const VM_DONTEXPAND = 0x00040000;
        const VM_LOCKONFAULT = 0x00080000;
        const VM_ACCOUNT = 0x00100000;
        const VM_NORESERVE = 0x00200000;
        const VM_HUGETLB = 0x00400000;
        const VM_SYNC = 0x00800000;
        const VM_ARCH_1 = 0x01000000;
        const VM_WIPEONFORK = 0x02000000;
        const VM_DONTDUMP = 0x04000000;
    }

    /// 描述页面错误处理过程中发生的不同情况或结果
        pub struct VmFaultReason:u32 {
        const VM_FAULT_OOM = 0x000001;
        const VM_FAULT_SIGBUS = 0x000002;
        const VM_FAULT_MAJOR = 0x000004;
        const VM_FAULT_WRITE = 0x000008;
        const VM_FAULT_HWPOISON = 0x000010;
        const VM_FAULT_HWPOISON_LARGE = 0x000020;
        const VM_FAULT_SIGSEGV = 0x000040;
        const VM_FAULT_NOPAGE = 0x000100;
        const VM_FAULT_LOCKED = 0x000200;
        const VM_FAULT_RETRY = 0x000400;
        const VM_FAULT_FALLBACK = 0x000800;
        const VM_FAULT_DONE_COW = 0x001000;
        const VM_FAULT_NEEDDSYNC = 0x002000;
        const VM_FAULT_COMPLETED = 0x004000;
        const VM_FAULT_HINDEX_MASK = 0x0f0000;
        const VM_FAULT_ERROR = 0x000001 | 0x000002 | 0x000040 | 0x000010 | 0x000020 | 0x000800;
    }

    pub struct MsFlags:usize {
        const MS_ASYNC = 1;
        const MS_INVALIDATE = 2;
        const MS_SYNC = 4;
    }
}

impl core::ops::Index<VmFlags> for [usize] {
    type Output = usize;

    fn index(&self, index: VmFlags) -> &Self::Output {
        &self[index.bits]
    }
}

impl core::ops::IndexMut<VmFlags> for [usize] {
    fn index_mut(&mut self, index: VmFlags) -> &mut Self::Output {
        &mut self[index.bits]
    }
}

/// 获取内核IDLE进程的用户地址空间结构体
#[allow(non_snake_case)]
#[inline(always)]
pub fn IDLE_PROCESS_ADDRESS_SPACE() -> Arc<AddressSpace> {
    unsafe {
        return __IDLE_PROCESS_ADDRESS_SPACE
            .as_ref()
            .expect("IDLE_PROCESS_ADDRESS_SPACE is null")
            .clone();
    }
}

/// 设置内核IDLE进程的用户地址空间结构体全局变量
#[allow(non_snake_case)]
pub unsafe fn set_IDLE_PROCESS_ADDRESS_SPACE(address_space: Arc<AddressSpace>) {
    static INITIALIZED: AtomicBool = AtomicBool::new(false);
    if INITIALIZED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::Acquire)
        .is_err()
    {
        panic!("IDLE_PROCESS_ADDRESS_SPACE is already initialized");
    }
    __IDLE_PROCESS_ADDRESS_SPACE = Some(address_space);
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Hash)]
pub enum PageTableKind {
    /// 用户可访问的页表
    User,
    /// 内核页表
    Kernel,
    /// x86内存虚拟化中使用的EPT
    #[cfg(target_arch = "x86_64")]
    EPT,
}

/// 物理内存地址
#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd, Hash, Default)]
#[repr(transparent)]
pub struct PhysAddr(usize);

impl PhysAddr {
    /// 最大物理地址
    pub const MAX: Self = PhysAddr(usize::MAX);

    #[inline(always)]
    pub const fn new(address: usize) -> Self {
        Self(address)
    }

    /// @brief 获取物理地址的值
    #[inline(always)]
    pub const fn data(&self) -> usize {
        self.0
    }

    /// @brief 将物理地址加上一个偏移量
    #[inline(always)]
    pub fn add(self, offset: usize) -> Self {
        Self(self.0 + offset)
    }

    /// @brief 判断物理地址是否按照指定要求对齐
    #[inline(always)]
    pub fn check_aligned(&self, align: usize) -> bool {
        return self.0 & (align - 1) == 0;
    }

    #[inline(always)]
    pub fn is_null(&self) -> bool {
        return self.0 == 0;
    }
}

impl Debug for PhysAddr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "PhysAddr({:#x})", self.0)
    }
}

impl core::ops::Add<usize> for PhysAddr {
    type Output = Self;

    #[inline(always)]
    fn add(self, rhs: usize) -> Self::Output {
        return Self(self.0 + rhs);
    }
}

impl core::ops::AddAssign<usize> for PhysAddr {
    #[inline(always)]
    fn add_assign(&mut self, rhs: usize) {
        self.0 += rhs;
    }
}

impl core::ops::Add<PhysAddr> for PhysAddr {
    type Output = Self;

    #[inline(always)]
    fn add(self, rhs: PhysAddr) -> Self::Output {
        return Self(self.0 + rhs.0);
    }
}

impl core::ops::AddAssign<PhysAddr> for PhysAddr {
    #[inline(always)]
    fn add_assign(&mut self, rhs: PhysAddr) {
        self.0 += rhs.0;
    }
}

impl core::ops::BitOrAssign<usize> for PhysAddr {
    #[inline(always)]
    fn bitor_assign(&mut self, rhs: usize) {
        self.0 |= rhs;
    }
}

impl core::ops::BitOrAssign<PhysAddr> for PhysAddr {
    #[inline(always)]
    fn bitor_assign(&mut self, rhs: PhysAddr) {
        self.0 |= rhs.0;
    }
}

impl core::ops::Sub<usize> for PhysAddr {
    type Output = Self;

    #[inline(always)]
    fn sub(self, rhs: usize) -> Self::Output {
        return Self(self.0 - rhs);
    }
}

impl core::ops::SubAssign<usize> for PhysAddr {
    #[inline(always)]
    fn sub_assign(&mut self, rhs: usize) {
        self.0 -= rhs;
    }
}

impl core::ops::Sub<PhysAddr> for PhysAddr {
    type Output = usize;

    #[inline(always)]
    fn sub(self, rhs: PhysAddr) -> Self::Output {
        return self.0 - rhs.0;
    }
}

impl core::ops::SubAssign<PhysAddr> for PhysAddr {
    #[inline(always)]
    fn sub_assign(&mut self, rhs: PhysAddr) {
        self.0 -= rhs.0;
    }
}

/// 虚拟内存地址
#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd, Hash, Default)]
#[repr(transparent)]
pub struct VirtAddr(usize);

impl VirtAddr {
    #[inline(always)]
    pub const fn new(address: usize) -> Self {
        return Self(address);
    }

    /// @brief 获取虚拟地址的值
    #[inline(always)]
    pub const fn data(&self) -> usize {
        return self.0;
    }

    /// @brief 判断虚拟地址的类型
    #[inline(always)]
    pub fn kind(&self) -> PageTableKind {
        if self.check_user() {
            return PageTableKind::User;
        } else {
            return PageTableKind::Kernel;
        }
    }

    /// @brief 判断虚拟地址是否按照指定要求对齐
    #[inline(always)]
    pub fn check_aligned(&self, align: usize) -> bool {
        return self.0 & (align - 1) == 0;
    }

    /// @brief 判断虚拟地址是否在用户空间
    #[inline(always)]
    pub fn check_user(&self) -> bool {
        return self < &MMArch::USER_END_VADDR;
    }

    #[inline(always)]
    pub fn as_ptr<T>(self) -> *mut T {
        return self.0 as *mut T;
    }

    #[inline(always)]
    pub fn is_null(&self) -> bool {
        return self.0 == 0;
    }
}

impl Add<VirtAddr> for VirtAddr {
    type Output = Self;

    #[inline(always)]
    fn add(self, rhs: VirtAddr) -> Self::Output {
        return Self(self.0 + rhs.0);
    }
}

impl Add<usize> for VirtAddr {
    type Output = Self;

    #[inline(always)]
    fn add(self, rhs: usize) -> Self::Output {
        return Self(self.0 + rhs);
    }
}

impl Sub<VirtAddr> for VirtAddr {
    type Output = usize;

    #[inline(always)]
    fn sub(self, rhs: VirtAddr) -> Self::Output {
        return self.0 - rhs.0;
    }
}

impl Sub<usize> for VirtAddr {
    type Output = Self;

    #[inline(always)]
    fn sub(self, rhs: usize) -> Self::Output {
        return Self(self.0 - rhs);
    }
}

impl AddAssign<usize> for VirtAddr {
    #[inline(always)]
    fn add_assign(&mut self, rhs: usize) {
        self.0 += rhs;
    }
}

impl AddAssign<VirtAddr> for VirtAddr {
    #[inline(always)]
    fn add_assign(&mut self, rhs: VirtAddr) {
        self.0 += rhs.0;
    }
}

impl SubAssign<usize> for VirtAddr {
    #[inline(always)]
    fn sub_assign(&mut self, rhs: usize) {
        self.0 -= rhs;
    }
}

impl SubAssign<VirtAddr> for VirtAddr {
    #[inline(always)]
    fn sub_assign(&mut self, rhs: VirtAddr) {
        self.0 -= rhs.0;
    }
}

impl Debug for VirtAddr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "VirtAddr({:#x})", self.0)
    }
}

/// @brief 物理内存区域
#[derive(Clone, Copy, Debug)]
pub struct PhysMemoryArea {
    /// 物理基地址
    pub base: PhysAddr,
    /// 该区域的物理内存大小
    pub size: usize,

    pub flags: MemoryAreaAttr,
}

impl PhysMemoryArea {
    pub const DEFAULT: Self = Self {
        base: PhysAddr::new(0),
        size: 0,
        flags: MemoryAreaAttr::empty(),
    };

    pub fn new(base: PhysAddr, size: usize, flags: MemoryAreaAttr) -> Self {
        Self { base, size, flags }
    }

    /// 返回向上页面对齐的区域起始物理地址
    pub fn area_base_aligned(&self) -> PhysAddr {
        return PhysAddr::new(
            (self.base.data() + (MMArch::PAGE_SIZE - 1)) & !(MMArch::PAGE_SIZE - 1),
        );
    }

    /// 返回向下页面对齐的区域截止物理地址
    pub fn area_end_aligned(&self) -> PhysAddr {
        return PhysAddr::new((self.base.data() + self.size) & !(MMArch::PAGE_SIZE - 1));
    }
}

impl Default for PhysMemoryArea {
    fn default() -> Self {
        return Self::DEFAULT;
    }
}

#[allow(dead_code)]
pub trait MemoryManagementArch: Clone + Copy + Debug {
    /// 是否支持缺页中断
    const PAGE_FAULT_ENABLED: bool;
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
    /// 页表项的write bit
    const ENTRY_FLAG_WRITEABLE: usize;
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
    /// 当该位为1时，标明这是一个脏页
    const ENTRY_FLAG_DIRTY: usize;
    /// 当该位为1时，代表这个页面被处理器访问过
    const ENTRY_FLAG_ACCESSED: usize;
    /// 标记该页表项指向的页是否为大页
    const ENTRY_FLAG_HUGE_PAGE: usize;
    /// 当该位为1时，代表该页表项是全局的
    const ENTRY_FLAG_GLOBAL: usize;

    /// 虚拟地址与物理地址的偏移量
    const PHYS_OFFSET: usize;

    /// 内核在链接时被链接到的偏移量
    const KERNEL_LINK_OFFSET: usize;

    const KERNEL_VIRT_START: usize = Self::PHYS_OFFSET + Self::KERNEL_LINK_OFFSET;

    /// 每个页面的大小
    const PAGE_SIZE: usize = 1 << Self::PAGE_SHIFT;
    /// 通过这个mask，获取地址的页内偏移量
    const PAGE_OFFSET_MASK: usize = Self::PAGE_SIZE - 1;
    /// 通过这个mask，获取页的首地址
    const PAGE_MASK: usize = !(Self::PAGE_OFFSET_MASK);
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
    /// 内核页表在顶级页表的第一个页表项的索引
    const PAGE_KERNEL_INDEX: usize = (Self::PHYS_OFFSET & Self::PAGE_ADDRESS_MASK)
        >> (Self::PAGE_ADDRESS_SHIFT - Self::PAGE_ENTRY_SHIFT);

    const PAGE_NEGATIVE_MASK: usize = !((Self::PAGE_ADDRESS_SIZE) - 1);

    const ENTRY_ADDRESS_SIZE: usize = 1 << Self::ENTRY_ADDRESS_SHIFT;
    /// 该mask用于获取页表项中地址字段
    const ENTRY_ADDRESS_MASK: usize = Self::ENTRY_ADDRESS_SIZE - Self::PAGE_SIZE;
    /// 这个mask用于获取页表项中的flags
    const ENTRY_FLAGS_MASK: usize = !Self::ENTRY_ADDRESS_MASK;

    /// 用户空间的最高地址
    const USER_END_VADDR: VirtAddr;
    /// 用户堆的起始地址
    const USER_BRK_START: VirtAddr;
    /// 用户栈起始地址（向下生长，不包含该值）
    const USER_STACK_START: VirtAddr;

    /// 内核的固定映射区的起始地址
    const FIXMAP_START_VADDR: VirtAddr;
    /// 内核的固定映射区的大小
    const FIXMAP_SIZE: usize;
    /// 内核的固定映射区的结束地址
    const FIXMAP_END_VADDR: VirtAddr =
        VirtAddr::new(Self::FIXMAP_START_VADDR.data() + Self::FIXMAP_SIZE);

    /// MMIO虚拟空间的基地址
    const MMIO_BASE: VirtAddr;
    /// MMIO虚拟空间的大小
    const MMIO_SIZE: usize;
    /// MMIO虚拟空间的顶端地址（不包含）
    const MMIO_TOP: VirtAddr = VirtAddr::new(Self::MMIO_BASE.data() + Self::MMIO_SIZE);

    /// @brief 用于初始化内存管理模块与架构相关的信息。
    /// 该函数应调用其他模块的接口，把可用内存区域添加到memblock，提供给BumpAllocator使用
    unsafe fn init();

    /// 内存管理初始化完成后，调用该函数
    unsafe fn arch_post_init() {}

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

    #[inline(always)]
    unsafe fn write_bytes(address: VirtAddr, value: u8, count: usize) {
        ptr::write_bytes(address.data() as *mut u8, value, count);
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

    /// 将虚拟地址转换为物理地址
    ///
    /// ## 参数
    ///
    /// - `virt` 虚拟地址
    ///
    /// ## 返回值
    ///
    /// 转换后的物理地址。如果转换失败，返回None
    #[inline(always)]
    unsafe fn virt_2_phys(virt: VirtAddr) -> Option<PhysAddr> {
        if let Some(paddr) = virt.data().checked_sub(Self::PHYS_OFFSET) {
            return Some(PhysAddr::new(paddr));
        } else {
            return None;
        }
    }

    /// @brief 判断指定的虚拟地址是否正确（符合规范）
    fn virt_is_valid(virt: VirtAddr) -> bool;

    /// 获取内存管理初始化时，创建的第一个内核页表的地址
    fn initial_page_table() -> PhysAddr;

    /// 初始化新的usermapper，为用户进程创建页表
    fn setup_new_usermapper() -> Result<UserMapper, SystemError>;

    /// 创建页表项
    ///
    /// 这是一个低阶api，用于根据物理地址以及指定好的EntryFlags，创建页表项
    ///
    /// ## 参数
    ///
    /// - `paddr` 物理地址
    /// - `page_flags` 页表项的flags
    ///
    /// ## 返回值
    ///
    /// 页表项的值
    fn make_entry(paddr: PhysAddr, page_flags: usize) -> usize;

    /// 判断一个VMA是否允许访问
    ///
    /// ## 参数
    ///
    /// - `vma`: 进行判断的VMA
    /// - `write`: 是否需要写入权限（true 表示需要写权限）
    /// - `execute`: 是否需要执行权限（true 表示需要执行权限）
    /// - `foreign`: 是否是外部的（即非当前进程的）VMA
    ///
    /// ## 返回值
    /// - `true`: VMA允许访问
    /// - `false`: 错误的说明
    fn vma_access_permitted(
        _vma: Arc<LockedVMA>,
        _write: bool,
        _execute: bool,
        _foreign: bool,
    ) -> bool {
        true
    }

    const PAGE_NONE: usize;
    const PAGE_SHARED: usize;
    const PAGE_SHARED_EXEC: usize;
    const PAGE_COPY_NOEXEC: usize;
    const PAGE_COPY_EXEC: usize;
    const PAGE_COPY: usize;
    const PAGE_READONLY: usize;
    const PAGE_READONLY_EXEC: usize;

    const PAGE_READ: usize;
    const PAGE_READ_EXEC: usize;
    const PAGE_WRITE: usize;
    const PAGE_WRITE_EXEC: usize;
    const PAGE_EXEC: usize;

    const PROTECTION_MAP: [EntryFlags<Self>; 16];

    /// 页面保护标志转换函数
    /// ## 参数
    ///
    /// - `vm_flags`: VmFlags标志
    ///
    /// ## 返回值
    /// - EntryFlags: 页面的保护位
    fn vm_get_page_prot(vm_flags: VmFlags) -> EntryFlags<Self> {
        let map = Self::PROTECTION_MAP;
        let mut ret = map[vm_flags
            .intersection(
                VmFlags::VM_READ | VmFlags::VM_WRITE | VmFlags::VM_EXEC | VmFlags::VM_SHARED,
            )
            .bits()];

        #[cfg(target_arch = "x86_64")]
        {
            // 如果xd位被保留，那么将可执行性设置为true
            if crate::arch::mm::X86_64MMArch::is_xd_reserved() {
                ret = ret.set_execute(true);
            }
        }
        ret
    }
}

/// @brief 虚拟地址范围
/// 该结构体用于表示一个虚拟地址范围，包括起始地址与大小
///
/// 请注意与VMA进行区分，该结构体被VMA所包含
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VirtRegion {
    start: VirtAddr,
    size: usize,
}

#[allow(dead_code)]
impl VirtRegion {
    /// # 创建一个新的虚拟地址范围
    pub fn new(start: VirtAddr, size: usize) -> Self {
        VirtRegion { start, size }
    }

    /// 获取虚拟地址范围的起始地址
    #[inline(always)]
    pub fn start(&self) -> VirtAddr {
        self.start
    }

    /// 获取虚拟地址范围的截止地址（不包括返回的地址）
    #[inline(always)]
    pub fn end(&self) -> VirtAddr {
        return self.start().add(self.size);
    }

    /// # Create a new VirtRegion from a range [start, end)
    ///
    /// If end <= start, return None
    pub fn between(start: VirtAddr, end: VirtAddr) -> Option<Self> {
        if unlikely(end.data() <= start.data()) {
            return None;
        }
        let size = end.data() - start.data();
        return Some(VirtRegion::new(start, size));
    }

    /// # 取两个虚拟地址范围的交集
    ///
    /// 如果两个虚拟地址范围没有交集，返回None
    pub fn intersect(&self, other: &VirtRegion) -> Option<VirtRegion> {
        let start = self.start.max(other.start);
        let end = self.end().min(other.end());
        return VirtRegion::between(start, end);
    }

    /// 设置虚拟地址范围的起始地址
    #[inline(always)]
    pub fn set_start(&mut self, start: VirtAddr) {
        self.start = start;
    }

    #[inline(always)]
    pub fn size(&self) -> usize {
        self.size
    }

    /// 设置虚拟地址范围的大小
    #[inline(always)]
    pub fn set_size(&mut self, size: usize) {
        self.size = size;
    }

    /// 判断虚拟地址范围是否为空
    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.size == 0
    }

    /// 将虚拟地址区域的大小向上对齐到页大小
    #[inline(always)]
    pub fn round_up_size_to_page(self) -> Self {
        return VirtRegion::new(self.start, round_up_to_page_size(self.size));
    }

    /// 判断两个虚拟地址范围是否由于具有交集而导致冲突
    #[inline(always)]
    pub fn collide(&self, other: &VirtRegion) -> bool {
        return self.intersect(other).is_some();
    }

    pub fn iter_pages(&self) -> VirtPageFrameIter {
        return VirtPageFrame::iter_range(
            VirtPageFrame::new(self.start),
            VirtPageFrame::new(self.end()),
        );
    }

    /// 获取[self.start(), region.start())的虚拟地址范围
    ///
    /// 如果self.start() >= region.start()，返回None
    pub fn before(self, region: &VirtRegion) -> Option<Self> {
        return Self::between(self.start(), region.start());
    }

    /// 获取[region.end(),self.end())的虚拟地址范围
    ///
    /// 如果 self.end() >= region.end() ，返回None
    pub fn after(self, region: &VirtRegion) -> Option<Self> {
        // if self.end() > region.end() none
        return Self::between(region.end(), self.end());
    }

    /// 把当前虚拟地址范围内的某个虚拟地址，转换为另一个虚拟地址范围内的虚拟地址
    ///
    /// 如果vaddr不在当前虚拟地址范围内，返回None
    ///
    /// 如果vaddr在当前虚拟地址范围内，返回vaddr在new_base中的虚拟地址
    pub fn rebase(self, vaddr: VirtAddr, new_base: &VirtRegion) -> Option<VirtAddr> {
        if !self.contains(vaddr) {
            return None;
        }
        let offset = vaddr.data() - self.start().data();
        let new_start = new_base.start().data() + offset;
        return Some(VirtAddr::new(new_start));
    }

    /// 判断虚拟地址范围是否包含指定的虚拟地址
    pub fn contains(&self, addr: VirtAddr) -> bool {
        return self.start() <= addr && addr < self.end();
    }

    /// 创建当前虚拟地址范围的页面迭代器
    pub fn pages(&self) -> VirtPageFrameIter {
        return VirtPageFrame::iter_range(
            VirtPageFrame::new(self.start()),
            VirtPageFrame::new(self.end()),
        );
    }
}

impl PartialOrd for VirtRegion {
    fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for VirtRegion {
    fn cmp(&self, other: &Self) -> cmp::Ordering {
        return self.start.cmp(&other.start);
    }
}

/// ## 判断虚拟地址是否超出了用户空间
///
/// 如果虚拟地址超出了用户空间，返回Err(SystemError::EFAULT).
/// 如果end < start，返回Err(SystemError::EOVERFLOW)
///
/// 否则返回Ok(())
pub fn verify_area(addr: VirtAddr, size: usize) -> Result<(), SystemError> {
    let end = addr.add(size);
    if unlikely(end.data() < addr.data()) {
        return Err(SystemError::EOVERFLOW);
    }

    if !addr.check_user() || !end.check_user() {
        return Err(SystemError::EFAULT);
    }

    return Ok(());
}
