use core::{
    fmt::{self, Debug, Error, Formatter},
    marker::PhantomData,
    mem,
    ops::Add,
    sync::atomic::{compiler_fence, Ordering},
};

use crate::{
    arch::{interrupt::ipi::send_ipi, MMArch},
    exception::ipi::{IpiKind, IpiTarget},
    kerror, kwarn,
};

use super::{
    allocator::page_frame::FrameAllocator, syscall::ProtFlags, MemoryManagementArch, PageTableKind,
    PhysAddr, VirtAddr,
};

#[derive(Debug)]
pub struct PageTable<Arch> {
    /// 当前页表表示的虚拟地址空间的起始地址
    base: VirtAddr,
    /// 当前页表所在的物理地址
    phys: PhysAddr,
    /// 当前页表的层级（请注意，最顶级页表的level为[Arch::PAGE_LEVELS - 1]）
    level: usize,
    phantom: PhantomData<Arch>,
}

#[allow(dead_code)]
impl<Arch: MemoryManagementArch> PageTable<Arch> {
    pub unsafe fn new(base: VirtAddr, phys: PhysAddr, level: usize) -> Self {
        Self {
            base,
            phys,
            level,
            phantom: PhantomData,
        }
    }

    /// 获取顶级页表
    ///
    /// ## 参数
    ///
    /// - table_kind 页表类型
    ///
    /// ## 返回值
    ///
    /// 返回顶级页表
    pub unsafe fn top_level_table(table_kind: PageTableKind) -> Self {
        return Self::new(
            VirtAddr::new(0),
            Arch::table(table_kind),
            Arch::PAGE_LEVELS - 1,
        );
    }

    /// 获取当前页表的物理地址
    #[inline(always)]
    pub fn phys(&self) -> PhysAddr {
        self.phys
    }

    /// 当前页表表示的虚拟地址空间的起始地址
    #[inline(always)]
    pub fn base(&self) -> VirtAddr {
        self.base
    }

    /// 获取当前页表的层级
    #[inline(always)]
    pub fn level(&self) -> usize {
        self.level
    }

    /// 获取当前页表自身所在的虚拟地址
    #[inline(always)]
    pub unsafe fn virt(&self) -> VirtAddr {
        return Arch::phys_2_virt(self.phys).unwrap();
    }

    /// 获取第i个页表项所表示的虚拟内存空间的起始地址
    pub fn entry_base(&self, i: usize) -> Option<VirtAddr> {
        if i < Arch::PAGE_ENTRY_NUM {
            let shift = self.level * Arch::PAGE_ENTRY_SHIFT + Arch::PAGE_SHIFT;
            return Some(self.base.add(i << shift));
        } else {
            return None;
        }
    }

    /// 获取当前页表的第i个页表项所在的虚拟地址（注意与entry_base进行区分）
    pub unsafe fn entry_virt(&self, i: usize) -> Option<VirtAddr> {
        if i < Arch::PAGE_ENTRY_NUM {
            return Some(self.virt().add(i * Arch::PAGE_ENTRY_SIZE));
        } else {
            return None;
        }
    }

    /// 获取当前页表的第i个页表项
    pub unsafe fn entry(&self, i: usize) -> Option<PageEntry<Arch>> {
        let entry_virt = self.entry_virt(i)?;
        return Some(PageEntry::from_usize(Arch::read::<usize>(entry_virt)));
    }

    /// 设置当前页表的第i个页表项
    pub unsafe fn set_entry(&self, i: usize, entry: PageEntry<Arch>) -> Option<()> {
        let entry_virt = self.entry_virt(i)?;
        Arch::write::<usize>(entry_virt, entry.data());
        return Some(());
    }

    /// 判断当前页表的第i个页表项是否已经填写了值
    ///
    /// ## 参数
    /// - Some(true) 如果已经填写了值
    /// - Some(false) 如果未填写值
    /// - None 如果i超出了页表项的范围
    pub fn entry_mapped(&self, i: usize) -> Option<bool> {
        let etv = unsafe { self.entry_virt(i) }?;
        if unsafe { Arch::read::<usize>(etv) } != 0 {
            return Some(true);
        } else {
            return Some(false);
        }
    }

    /// 根据虚拟地址，获取对应的页表项在页表中的下标
    ///
    /// ## 参数
    ///
    /// - addr: 虚拟地址
    ///
    /// ## 返回值
    ///
    /// 页表项在页表中的下标。如果addr不在当前页表所表示的虚拟地址空间中，则返回None
    pub unsafe fn index_of(&self, addr: VirtAddr) -> Option<usize> {
        let addr = VirtAddr::new(addr.data() & Arch::PAGE_ADDRESS_MASK);
        let shift = self.level * Arch::PAGE_ENTRY_SHIFT + Arch::PAGE_SHIFT;

        let mask = (MMArch::PAGE_ENTRY_NUM << shift) - 1;
        if addr < self.base || addr >= self.base.add(mask) {
            return None;
        } else {
            return Some((addr.data() >> shift) & MMArch::PAGE_ENTRY_MASK);
        }
    }

    /// 获取第i个页表项指向的下一级页表
    pub unsafe fn next_level_table(&self, index: usize) -> Option<Self> {
        if self.level == 0 {
            return None;
        }

        // 返回下一级页表
        return Some(PageTable::new(
            self.entry_base(index)?,
            self.entry(index)?.address().ok()?,
            self.level - 1,
        ));
    }
}

/// 页表项
#[derive(Copy, Clone)]
pub struct PageEntry<Arch> {
    data: usize,
    phantom: PhantomData<Arch>,
}

impl<Arch> Debug for PageEntry<Arch> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), Error> {
        f.write_fmt(format_args!("PageEntry({:#x})", self.data))
    }
}

impl<Arch: MemoryManagementArch> PageEntry<Arch> {
    #[inline(always)]
    pub fn new(paddr: PhysAddr, flags: PageFlags<Arch>) -> Self {
        Self {
            data: MMArch::make_entry(paddr, flags.data()),
            phantom: PhantomData,
        }
    }
    #[inline(always)]
    pub fn from_usize(data: usize) -> Self {
        Self {
            data,
            phantom: PhantomData,
        }
    }

    #[inline(always)]
    pub fn data(&self) -> usize {
        self.data
    }

    /// 获取当前页表项指向的物理地址
    ///
    /// ## 返回值
    ///
    /// - Ok(PhysAddr) 如果当前页面存在于物理内存中, 返回物理地址
    /// - Err(PhysAddr) 如果当前页表项不存在, 返回物理地址
    #[inline(always)]
    pub fn address(&self) -> Result<PhysAddr, PhysAddr> {
        let paddr: PhysAddr = {
            #[cfg(target_arch = "x86_64")]
            {
                PhysAddr::new(self.data & Arch::PAGE_ADDRESS_MASK)
            }

            #[cfg(target_arch = "riscv64")]
            {
                let ppn = ((self.data & (!((1 << 10) - 1))) >> 10) & ((1 << 54) - 1);
                super::allocator::page_frame::PhysPageFrame::from_ppn(ppn).phys_address()
            }
        };

        if self.present() {
            Ok(paddr)
        } else {
            Err(paddr)
        }
    }

    #[inline(always)]
    pub fn flags(&self) -> PageFlags<Arch> {
        unsafe { PageFlags::from_data(self.data & Arch::ENTRY_FLAGS_MASK) }
    }

    #[inline(always)]
    pub fn set_flags(&mut self, flags: PageFlags<Arch>) {
        self.data = (self.data & !Arch::ENTRY_FLAGS_MASK) | flags.data();
    }

    #[inline(always)]
    pub fn present(&self) -> bool {
        return self.data & Arch::ENTRY_FLAG_PRESENT != 0;
    }
}

/// 页表项的标志位
#[derive(Copy, Clone, Hash)]
pub struct PageFlags<Arch> {
    data: usize,
    phantom: PhantomData<Arch>,
}

#[allow(dead_code)]
impl<Arch: MemoryManagementArch> PageFlags<Arch> {
    #[inline(always)]
    pub fn new() -> Self {
        let mut r = unsafe {
            Self::from_data(
                Arch::ENTRY_FLAG_DEFAULT_PAGE
                    | Arch::ENTRY_FLAG_READONLY
                    | Arch::ENTRY_FLAG_NO_EXEC,
            )
        };

        #[cfg(target_arch = "x86_64")]
        {
            if crate::arch::mm::X86_64MMArch::is_xd_reserved() {
                r = r.set_execute(true);
            }
        }

        return r;
    }

    /// 根据ProtFlags生成PageFlags
    ///
    /// ## 参数
    ///
    /// - prot_flags: 页的保护标志
    /// - user: 用户空间是否可访问
    pub fn from_prot_flags(prot_flags: ProtFlags, user: bool) -> PageFlags<Arch> {
        let flags: PageFlags<Arch> = PageFlags::new()
            .set_user(user)
            .set_execute(prot_flags.contains(ProtFlags::PROT_EXEC))
            .set_write(prot_flags.contains(ProtFlags::PROT_WRITE));

        return flags;
    }

    #[inline(always)]
    pub fn data(&self) -> usize {
        self.data
    }

    #[inline(always)]
    pub const unsafe fn from_data(data: usize) -> Self {
        return Self {
            data: data,
            phantom: PhantomData,
        };
    }

    /// 为新页表的页表项设置默认值
    ///
    /// 默认值为：
    /// - present
    /// - read only
    /// - kernel space
    /// - no exec
    #[inline(always)]
    pub fn new_page_table(user: bool) -> Self {
        return unsafe {
            let r = {
                #[cfg(target_arch = "x86_64")]
                {
                    Self::from_data(Arch::ENTRY_FLAG_DEFAULT_TABLE | Arch::ENTRY_FLAG_READWRITE)
                }

                #[cfg(target_arch = "riscv64")]
                {
                    // riscv64指向下一级页表的页表项，不应设置R/W/X权限位
                    Self::from_data(Arch::ENTRY_FLAG_DEFAULT_TABLE)
                }
            };
            if user {
                r.set_user(true)
            } else {
                r
            }
        };
    }

    /// 取得当前页表项的所有权，更新当前页表项的标志位，并返回更新后的页表项。
    ///
    /// ## 参数
    /// - flag 要更新的标志位的值
    /// - value 如果为true，那么将flag对应的位设置为1，否则设置为0
    ///
    /// ## 返回值
    ///
    /// 更新后的页表项
    #[inline(always)]
    #[must_use]
    pub fn update_flags(mut self, flag: usize, value: bool) -> Self {
        if value {
            self.data |= flag;
        } else {
            self.data &= !flag;
        }
        return self;
    }

    /// 判断当前页表项是否存在指定的flag（只有全部flag都存在才返回true）
    #[inline(always)]
    pub fn has_flag(&self, flag: usize) -> bool {
        return self.data & flag == flag;
    }

    #[inline(always)]
    pub fn present(&self) -> bool {
        return self.has_flag(Arch::ENTRY_FLAG_PRESENT);
    }

    /// 设置当前页表项的权限
    ///
    /// @param value 如果为true，那么将当前页表项的权限设置为用户态可访问
    #[must_use]
    #[inline(always)]
    pub fn set_user(self, value: bool) -> Self {
        return self.update_flags(Arch::ENTRY_FLAG_USER, value);
    }

    /// 用户态是否可以访问当前页表项
    #[inline(always)]
    pub fn has_user(&self) -> bool {
        return self.has_flag(Arch::ENTRY_FLAG_USER);
    }

    /// 设置当前页表项的可写性, 如果为true，那么将当前页表项的权限设置为可写, 否则设置为只读
    ///
    /// ## 返回值
    ///
    /// 更新后的页表项.
    ///
    /// **请注意，**本函数会取得当前页表项的所有权，因此返回的页表项不是原来的页表项
    #[must_use]
    #[inline(always)]
    pub fn set_write(self, value: bool) -> Self {
        #[cfg(target_arch = "x86_64")]
        {
            // 有的架构同时具有可写和不可写的标志位，因此需要同时更新
            return self
                .update_flags(Arch::ENTRY_FLAG_READONLY, !value)
                .update_flags(Arch::ENTRY_FLAG_READWRITE, value);
        }

        #[cfg(target_arch = "riscv64")]
        {
            if value {
                return self.update_flags(Arch::ENTRY_FLAG_READWRITE, true);
            } else {
                return self.update_flags(Arch::ENTRY_FLAG_READONLY, true);
            }
        }
    }

    /// 当前页表项是否可写
    #[inline(always)]
    pub fn has_write(&self) -> bool {
        // 有的架构同时具有可写和不可写的标志位，因此需要同时判断
        return self.data & (Arch::ENTRY_FLAG_READWRITE | Arch::ENTRY_FLAG_READONLY)
            == Arch::ENTRY_FLAG_READWRITE;
    }

    /// 设置当前页表项的可执行性, 如果为true，那么将当前页表项的权限设置为可执行, 否则设置为不可执行
    #[must_use]
    #[inline(always)]
    pub fn set_execute(self, mut value: bool) -> Self {
        #[cfg(target_arch = "x86_64")]
        {
            // 如果xd位被保留，那么将可执行性设置为true
            if crate::arch::mm::X86_64MMArch::is_xd_reserved() {
                value = true;
            }
        }

        // 有的架构同时具有可执行和不可执行的标志位，因此需要同时更新
        return self
            .update_flags(Arch::ENTRY_FLAG_NO_EXEC, !value)
            .update_flags(Arch::ENTRY_FLAG_EXEC, value);
    }

    /// 当前页表项是否可执行
    #[inline(always)]
    pub fn has_execute(&self) -> bool {
        // 有的架构同时具有可执行和不可执行的标志位，因此需要同时判断
        return self.data & (Arch::ENTRY_FLAG_EXEC | Arch::ENTRY_FLAG_NO_EXEC)
            == Arch::ENTRY_FLAG_EXEC;
    }

    /// 设置当前页表项的缓存策略
    ///
    /// ## 参数
    ///
    /// - value: 如果为true，那么将当前页表项的缓存策略设置为不缓存。
    #[inline(always)]
    pub fn set_page_cache_disable(self, value: bool) -> Self {
        return self.update_flags(Arch::ENTRY_FLAG_CACHE_DISABLE, value);
    }

    /// 获取当前页表项的缓存策略
    ///
    /// ## 返回值
    ///
    /// 如果当前页表项的缓存策略为不缓存，那么返回true，否则返回false。
    #[inline(always)]
    pub fn has_page_cache_disable(&self) -> bool {
        return self.has_flag(Arch::ENTRY_FLAG_CACHE_DISABLE);
    }

    /// 设置当前页表项的写穿策略
    ///
    /// ## 参数
    ///
    /// - value: 如果为true，那么将当前页表项的写穿策略设置为写穿。
    #[inline(always)]
    pub fn set_page_write_through(self, value: bool) -> Self {
        return self.update_flags(Arch::ENTRY_FLAG_WRITE_THROUGH, value);
    }

    /// 获取当前页表项的写穿策略
    ///
    /// ## 返回值
    ///
    /// 如果当前页表项的写穿策略为写穿，那么返回true，否则返回false。
    #[inline(always)]
    pub fn has_page_write_through(&self) -> bool {
        return self.has_flag(Arch::ENTRY_FLAG_WRITE_THROUGH);
    }

    /// MMIO内存的页表项标志
    #[inline(always)]
    pub fn mmio_flags() -> Self {
        return Self::new()
            .set_user(false)
            .set_write(true)
            .set_execute(true)
            .set_page_cache_disable(true)
            .set_page_write_through(true);
    }
}

impl<Arch: MemoryManagementArch> fmt::Debug for PageFlags<Arch> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PageFlags")
            .field("bits", &format_args!("{:#0x}", self.data))
            .field("present", &self.present())
            .field("has_write", &self.has_write())
            .field("has_execute", &self.has_execute())
            .field("has_user", &self.has_user())
            .finish()
    }
}

/// 页表映射器
#[derive(Hash)]
pub struct PageMapper<Arch, F> {
    /// 页表类型
    table_kind: PageTableKind,
    /// 根页表物理地址
    table_paddr: PhysAddr,
    /// 页分配器
    frame_allocator: F,
    phantom: PhantomData<fn() -> Arch>,
}

impl<Arch: MemoryManagementArch, F: FrameAllocator> PageMapper<Arch, F> {
    /// 创建新的页面映射器
    ///
    /// ## 参数
    /// - table_kind 页表类型
    /// - table_paddr 根页表物理地址
    /// - allocator 页分配器
    ///
    /// ## 返回值
    ///
    /// 页面映射器
    pub unsafe fn new(table_kind: PageTableKind, table_paddr: PhysAddr, allocator: F) -> Self {
        return Self {
            table_kind,
            table_paddr,
            frame_allocator: allocator,
            phantom: PhantomData,
        };
    }

    /// 创建页表，并为这个页表创建页面映射器
    pub unsafe fn create(table_kind: PageTableKind, mut allocator: F) -> Option<Self> {
        let table_paddr = allocator.allocate_one()?;
        // 清空页表
        let table_vaddr = Arch::phys_2_virt(table_paddr)?;
        Arch::write_bytes(table_vaddr, 0, Arch::PAGE_SIZE);
        return Some(Self::new(table_kind, table_paddr, allocator));
    }

    /// 获取当前页表的页面映射器
    #[inline(always)]
    pub unsafe fn current(table_kind: PageTableKind, allocator: F) -> Self {
        let table_paddr = Arch::table(table_kind);
        return Self::new(table_kind, table_paddr, allocator);
    }

    /// 判断当前页表分配器所属的页表是否是当前页表
    #[inline(always)]
    pub fn is_current(&self) -> bool {
        return unsafe { self.table().phys() == Arch::table(self.table_kind) };
    }

    /// 将当前页表分配器所属的页表设置为当前页表
    #[inline(always)]
    pub unsafe fn make_current(&self) {
        Arch::set_table(self.table_kind, self.table_paddr);
    }

    /// 获取当前页表分配器所属的根页表的结构体
    #[inline(always)]
    pub fn table(&self) -> PageTable<Arch> {
        // 由于只能通过new方法创建PageMapper，因此这里假定table_paddr是有效的
        return unsafe {
            PageTable::new(VirtAddr::new(0), self.table_paddr, Arch::PAGE_LEVELS - 1)
        };
    }

    /// 获取当前PageMapper所对应的页分配器实例的引用
    #[inline(always)]
    #[allow(dead_code)]
    pub fn allocator_ref(&self) -> &F {
        return &self.frame_allocator;
    }

    /// 获取当前PageMapper所对应的页分配器实例的可变引用
    #[inline(always)]
    pub fn allocator_mut(&mut self) -> &mut F {
        return &mut self.frame_allocator;
    }

    /// 从当前PageMapper的页分配器中分配一个物理页，并将其映射到指定的虚拟地址
    pub unsafe fn map(
        &mut self,
        virt: VirtAddr,
        flags: PageFlags<Arch>,
    ) -> Option<PageFlush<Arch>> {
        compiler_fence(Ordering::SeqCst);
        let phys: PhysAddr = self.frame_allocator.allocate_one()?;
        compiler_fence(Ordering::SeqCst);
        return self.map_phys(virt, phys, flags);
    }

    /// 映射一个物理页到指定的虚拟地址
    pub unsafe fn map_phys(
        &mut self,
        virt: VirtAddr,
        phys: PhysAddr,
        flags: PageFlags<Arch>,
    ) -> Option<PageFlush<Arch>> {
        // 验证虚拟地址和物理地址是否对齐
        if !(virt.check_aligned(Arch::PAGE_SIZE) && phys.check_aligned(Arch::PAGE_SIZE)) {
            kerror!(
                "Try to map unaligned page: virt={:?}, phys={:?}",
                virt,
                phys
            );
            return None;
        }

        let virt = VirtAddr::new(virt.data() & (!Arch::PAGE_NEGATIVE_MASK));

        // TODO： 验证flags是否合法

        // 创建页表项
        let entry = PageEntry::new(phys, flags);
        let mut table = self.table();
        loop {
            let i = table.index_of(virt)?;
            assert!(i < Arch::PAGE_ENTRY_NUM);
            if table.level() == 0 {
                // todo: 检查是否已经映射
                // 现在不检查的原因是，刚刚启动系统时，内核会映射一些页。
                if table.entry_mapped(i)? == true {
                    kwarn!("Page {:?} already mapped", virt);
                }

                compiler_fence(Ordering::SeqCst);

                table.set_entry(i, entry);
                compiler_fence(Ordering::SeqCst);
                return Some(PageFlush::new(virt));
            } else {
                let next_table = table.next_level_table(i);
                if let Some(next_table) = next_table {
                    table = next_table;
                    // kdebug!("Mapping {:?} to next level table...", virt);
                } else {
                    // 分配下一级页表
                    let frame = self.frame_allocator.allocate_one()?;

                    // 清空这个页帧
                    MMArch::write_bytes(MMArch::phys_2_virt(frame).unwrap(), 0, MMArch::PAGE_SIZE);

                    // 设置页表项的flags
                    let flags: PageFlags<Arch> =
                        PageFlags::new_page_table(virt.kind() == PageTableKind::User);

                    // kdebug!("Flags: {:?}", flags);

                    // 把新分配的页表映射到当前页表
                    table.set_entry(i, PageEntry::new(frame, flags));

                    // 获取新分配的页表
                    table = table.next_level_table(i)?;
                }
            }
        }
    }

    /// 将物理地址映射到具有线性偏移量的虚拟地址
    #[allow(dead_code)]
    pub unsafe fn map_linearly(
        &mut self,
        phys: PhysAddr,
        flags: PageFlags<Arch>,
    ) -> Option<(VirtAddr, PageFlush<Arch>)> {
        let virt: VirtAddr = Arch::phys_2_virt(phys)?;
        return self.map_phys(virt, phys, flags).map(|flush| (virt, flush));
    }

    /// 修改虚拟地址的页表项的flags，并返回页表项刷新器
    ///
    /// 请注意，需要在修改完flags后，调用刷新器的flush方法，才能使修改生效
    ///
    /// ## 参数
    /// - virt 虚拟地址
    /// - flags 新的页表项的flags
    ///
    /// ## 返回值
    ///
    /// 如果修改成功，返回刷新器，否则返回None
    pub unsafe fn remap(
        &mut self,
        virt: VirtAddr,
        flags: PageFlags<Arch>,
    ) -> Option<PageFlush<Arch>> {
        return self
            .visit(virt, |p1, i| {
                let mut entry = p1.entry(i)?;
                entry.set_flags(flags);
                p1.set_entry(i, entry);
                Some(PageFlush::new(virt))
            })
            .flatten();
    }

    /// 根据虚拟地址，查找页表，获取对应的物理地址和页表项的flags
    ///
    /// ## 参数
    ///
    /// - virt 虚拟地址
    ///
    /// ## 返回值
    ///
    /// 如果查找成功，返回物理地址和页表项的flags，否则返回None
    pub fn translate(&self, virt: VirtAddr) -> Option<(PhysAddr, PageFlags<Arch>)> {
        let entry: PageEntry<Arch> = self.visit(virt, |p1, i| unsafe { p1.entry(i) })??;
        let paddr = entry.address().ok()?;
        let flags = entry.flags();
        return Some((paddr, flags));
    }

    /// 取消虚拟地址的映射，释放页面，并返回页表项刷新器
    ///
    /// 请注意，需要在取消映射后，调用刷新器的flush方法，才能使修改生效
    ///
    /// ## 参数
    ///
    /// - virt 虚拟地址
    /// - unmap_parents 是否在父页表内，取消空闲子页表的映射
    ///
    /// ## 返回值
    /// 如果取消成功，返回刷新器，否则返回None
    #[allow(dead_code)]
    pub unsafe fn unmap(&mut self, virt: VirtAddr, unmap_parents: bool) -> Option<PageFlush<Arch>> {
        let (paddr, _, flusher) = self.unmap_phys(virt, unmap_parents)?;
        self.frame_allocator.free_one(paddr);
        return Some(flusher);
    }

    /// 取消虚拟地址的映射，并返回物理地址和页表项的flags
    ///
    /// ## 参数
    ///
    /// - vaddr 虚拟地址
    /// - unmap_parents 是否在父页表内，取消空闲子页表的映射
    ///
    /// ## 返回值
    ///
    /// 如果取消成功，返回物理地址和页表项的flags，否则返回None
    pub unsafe fn unmap_phys(
        &mut self,
        virt: VirtAddr,
        unmap_parents: bool,
    ) -> Option<(PhysAddr, PageFlags<Arch>, PageFlush<Arch>)> {
        if !virt.check_aligned(Arch::PAGE_SIZE) {
            kerror!("Try to unmap unaligned page: virt={:?}", virt);
            return None;
        }

        let mut table = self.table();
        return unmap_phys_inner(virt, &mut table, unmap_parents, self.allocator_mut())
            .map(|(paddr, flags)| (paddr, flags, PageFlush::<Arch>::new(virt)));
    }

    /// 在页表中，访问虚拟地址对应的页表项，并调用传入的函数F
    fn visit<T>(
        &self,
        virt: VirtAddr,
        f: impl FnOnce(&mut PageTable<Arch>, usize) -> T,
    ) -> Option<T> {
        let mut table = self.table();
        unsafe {
            loop {
                let i = table.index_of(virt)?;
                if table.level() == 0 {
                    return Some(f(&mut table, i));
                } else {
                    table = table.next_level_table(i)?;
                }
            }
        }
    }
}

/// 取消页面映射，返回被取消映射的页表项的：【物理地址】和【flags】
///
/// ## 参数
///
/// - vaddr 虚拟地址
/// - table 页表
/// - unmap_parents 是否在父页表内，取消空闲子页表的映射
/// - allocator 页面分配器（如果页表从这个分配器分配，那么在取消映射时，也需要归还到这个分配器内）
///
/// ## 返回值
///
/// 如果取消成功，返回被取消映射的页表项的：【物理地址】和【flags】，否则返回None
unsafe fn unmap_phys_inner<Arch: MemoryManagementArch>(
    vaddr: VirtAddr,
    table: &PageTable<Arch>,
    unmap_parents: bool,
    allocator: &mut impl FrameAllocator,
) -> Option<(PhysAddr, PageFlags<Arch>)> {
    // 获取页表项的索引
    let i = table.index_of(vaddr)?;

    // 如果当前是最后一级页表，直接取消页面映射
    if table.level() == 0 {
        let entry = table.entry(i)?;
        table.set_entry(i, PageEntry::from_usize(0));
        return Some((entry.address().ok()?, entry.flags()));
    }

    let mut subtable = table.next_level_table(i)?;
    // 递归地取消映射
    let result = unmap_phys_inner(vaddr, &mut subtable, unmap_parents, allocator)?;

    // TODO: This is a bad idea for architectures where the kernel mappings are done in the process tables,
    // as these mappings may become out of sync
    if unmap_parents {
        // 如果子页表已经没有映射的页面了，就取消子页表的映射

        // 检查子页表中是否还有映射的页面
        let x = (0..Arch::PAGE_ENTRY_NUM)
            .map(|k| subtable.entry(k).expect("invalid page entry"))
            .any(|e| e.present());
        if !x {
            // 如果没有，就取消子页表的映射
            table.set_entry(i, PageEntry::from_usize(0));
            // 释放子页表
            allocator.free_one(subtable.phys());
        }
    }

    return Some(result);
}

impl<Arch, F: Debug> Debug for PageMapper<Arch, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PageMapper")
            .field("table_paddr", &self.table_paddr)
            .field("frame_allocator", &self.frame_allocator)
            .finish()
    }
}

/// 页表刷新器的trait
pub trait Flusher<Arch: MemoryManagementArch> {
    /// 取消对指定的page flusher的刷新
    fn consume(&mut self, flush: PageFlush<Arch>);
}

/// 用于刷新某个虚拟地址的刷新器。这个刷新器一经产生，就必须调用flush()方法，
/// 否则会造成对页表的更改被忽略，这是不安全的
#[must_use = "The flusher must call the 'flush()', or the changes to page table will be unsafely ignored."]
pub struct PageFlush<Arch: MemoryManagementArch> {
    virt: VirtAddr,
    phantom: PhantomData<Arch>,
}

impl<Arch: MemoryManagementArch> PageFlush<Arch> {
    pub fn new(virt: VirtAddr) -> Self {
        return Self {
            virt,
            phantom: PhantomData,
        };
    }

    pub fn flush(self) {
        unsafe { Arch::invalidate_page(self.virt) };
    }

    /// 忽略掉这个刷新器
    pub unsafe fn ignore(self) {
        mem::forget(self);
    }
}

impl<Arch: MemoryManagementArch> Drop for PageFlush<Arch> {
    fn drop(&mut self) {
        unsafe {
            MMArch::invalidate_page(self.virt);
        }
    }
}

/// 用于刷新整个页表的刷新器。这个刷新器一经产生，就必须调用flush()方法，
/// 否则会造成对页表的更改被忽略，这是不安全的
#[must_use = "The flusher must call the 'flush()', or the changes to page table will be unsafely ignored."]
pub struct PageFlushAll<Arch: MemoryManagementArch> {
    phantom: PhantomData<fn() -> Arch>,
}

#[allow(dead_code)]
impl<Arch: MemoryManagementArch> PageFlushAll<Arch> {
    pub fn new() -> Self {
        return Self {
            phantom: PhantomData,
        };
    }

    pub fn flush(self) {
        unsafe { Arch::invalidate_all() };
    }

    /// 忽略掉这个刷新器
    pub unsafe fn ignore(self) {
        mem::forget(self);
    }
}

impl<Arch: MemoryManagementArch> Flusher<Arch> for PageFlushAll<Arch> {
    /// 为page flush all 实现consume，消除对单个页面的刷新。（刷新整个页表了就不需要刷新单个页面了）
    fn consume(&mut self, flush: PageFlush<Arch>) {
        unsafe { flush.ignore() };
    }
}

impl<Arch: MemoryManagementArch, T: Flusher<Arch> + ?Sized> Flusher<Arch> for &mut T {
    /// 允许一个flusher consume掉另一个flusher
    fn consume(&mut self, flush: PageFlush<Arch>) {
        <T as Flusher<Arch>>::consume(self, flush);
    }
}

impl<Arch: MemoryManagementArch> Flusher<Arch> for () {
    fn consume(&mut self, _flush: PageFlush<Arch>) {}
}

impl<Arch: MemoryManagementArch> Drop for PageFlushAll<Arch> {
    fn drop(&mut self) {
        unsafe {
            Arch::invalidate_all();
        }
    }
}

/// 未在当前CPU上激活的页表的刷新器
///
/// 如果页表没有在当前cpu上激活，那么需要发送ipi到其他核心，尝试在其他核心上刷新页表
///
/// TODO: 这个方式很暴力，也许把它改成在指定的核心上刷新页表会更好。（可以测试一下开销）
#[derive(Debug)]
pub struct InactiveFlusher;

impl InactiveFlusher {
    pub fn new() -> Self {
        return Self {};
    }
}

impl Flusher<MMArch> for InactiveFlusher {
    fn consume(&mut self, flush: PageFlush<MMArch>) {
        unsafe {
            flush.ignore();
        }
    }
}

impl Drop for InactiveFlusher {
    fn drop(&mut self) {
        // 发送刷新页表的IPI
        send_ipi(IpiKind::FlushTLB, IpiTarget::Other);
    }
}

/// # 把一个地址向下对齐到页大小
pub fn round_down_to_page_size(addr: usize) -> usize {
    addr & !(MMArch::PAGE_SIZE - 1)
}

/// # 把一个地址向上对齐到页大小
pub fn round_up_to_page_size(addr: usize) -> usize {
    round_down_to_page_size(addr + MMArch::PAGE_SIZE - 1)
}
