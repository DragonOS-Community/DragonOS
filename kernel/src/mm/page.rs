use core::{fmt, marker::PhantomData};

use super::{MemoryManagementArch, PageTableKind, PhysAddr, VirtAddr};

pub struct PageTable<Arch> {
    /// 当前页表表示的虚拟地址空间的起始地址
    base: VirtAddr,
    /// 当前页表所在的物理地址
    phys: PhysAddr,
    /// 当前页表的层级（请注意，最顶级页表的level为[Arch::PAGE_LEVELS - 1]）
    level: usize,
    phantom: PhantomData<Arch>,
}

impl<Arch: MemoryManagementArch> PageTable<Arch> {
    pub unsafe fn new(base: VirtAddr, phys: PhysAddr, level: usize) -> Self {
        Self {
            base,
            phys,
            level,
            phantom: PhantomData,
        }
    }

    /// @brief 获取顶级页表
    ///
    /// @param table_kind 页表类型
    ///
    /// @return 顶级页表
    pub unsafe fn top_level_table(table_kind: PageTableKind) -> Self {
        return Self::new(
            VirtAddr::new(0),
            Arch::table(table_kind),
            Arch::PAGE_LEVELS - 1,
        );
    }

    /// @brief 获取当前页表的物理地址
    #[inline(always)]
    pub fn phys(&self) -> PhysAddr {
        self.phys
    }

    /// @brief 获取当前页表表示的内存空间的起始地址
    #[inline(always)]
    pub fn base(&self) -> VirtAddr {
        self.base
    }

    /// @brief 获取当前页表的层级
    #[inline(always)]
    pub fn level(&self) -> usize {
        self.level
    }

    /// @brief 获取当前页表自身所在的虚拟地址
    #[inline(always)]
    pub unsafe fn virt(&self) -> VirtAddr {
        return Arch::phys_2_virt(self.phys).unwrap();
    }

    /// @brief 获取第i个页表项所表示的虚拟内存空间的起始地址
    pub fn entry_base(&self, i: usize) -> Option<VirtAddr> {
        if i < Arch::PAGE_ENTRY_NUM {
            let shift = self.level * Arch::PAGE_ENTRY_SHIFT + Arch::PAGE_SHIFT;
            return Some(self.base.add(i << shift));
        } else {
            return None;
        }
    }

    /// @brief 获取当前页表的第i个页表项所在的虚拟地址（注意与entry_base进行区分）
    pub unsafe fn entry_virt(&self, i: usize) -> Option<VirtAddr> {
        if i < Arch::PAGE_ENTRY_NUM {
            return Some(self.virt().add(i * Arch::PAGE_ENTRY_SIZE));
        } else {
            return None;
        }
    }

    /// @brief 获取当前页表的第i个页表项
    pub unsafe fn entry(&self, i: usize) -> Option<PageEntry<Arch>> {
        let entry_virt = self.entry_virt(i)?;
        return Some(PageEntry::new(Arch::read::<usize>(entry_virt)));
    }

    /// @brief 设置当前页表的第i个页表项
    pub unsafe fn set_entry(&self, i: usize, entry: PageEntry<Arch>) -> Option<()> {
        let entry_virt = self.entry_virt(i)?;
        Arch::write::<usize>(entry_virt, entry.data());
        return Some(());
    }

    /// @brief 根据虚拟地址，获取对应的页表项在页表中的下标
    ///
    /// @param addr 虚拟地址
    ///
    /// @return 页表项在页表中的下标。如果addr不在当前页表所表示的虚拟地址空间中，则返回None
    pub unsafe fn index_of(&self, addr: VirtAddr) -> Option<usize> {
        let addr = VirtAddr::new(addr.data() & Arch::PAGE_ADDRESS_MASK);
        let shift = self.level * Arch::PAGE_ENTRY_SHIFT + Arch::PAGE_SHIFT;

        let index = addr.data() >> shift;
        if index >= Arch::PAGE_ENTRY_NUM {
            return None;
        }
        return Some(index & Arch::PAGE_ENTRY_MASK);
    }

    /// @brief 获取第i个页表项指向的下一级页表
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
#[derive(Debug, Copy, Clone)]
pub struct PageEntry<Arch> {
    data: usize,
    phantom: PhantomData<Arch>,
}

impl<Arch: MemoryManagementArch> PageEntry<Arch> {
    #[inline(always)]
    pub fn new(data: usize) -> Self {
        Self {
            data,
            phantom: PhantomData,
        }
    }

    #[inline(always)]
    pub fn data(&self) -> usize {
        self.data
    }

    /// @brief 获取当前页表项指向的物理地址
    ///
    /// @return Ok(PhysAddr) 如果当前页面存在, 返回物理地址
    /// @return Err(PhysAddr) 如果当前页表项不存在, 返回物理地址
    #[inline(always)]
    pub fn address(&self) -> Result<PhysAddr, PhysAddr> {
        let paddr = PhysAddr::new(self.data & Arch::PAGE_ADDRESS_MASK);

        if self.present() {
            Ok(paddr)
        } else {
            Err(paddr)
        }
    }

    #[inline(always)]
    pub fn flags(&self) -> PageFlags<Arch> {
        PageFlags::new(self.data & Arch::ENTRY_FLAGS_MASK)
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
#[derive(Copy, Clone)]
pub struct PageFlags<Arch> {
    data: usize,
    phantom: PhantomData<Arch>,
}

impl<Arch: MemoryManagementArch> PageFlags<Arch> {
    #[inline(always)]
    pub fn new(data: usize) -> Self {
        return unsafe { Self::from_data(data) };
    }

    #[inline(always)]
    pub fn data(&self) -> usize {
        self.data
    }

    #[inline(always)]
    pub unsafe fn from_data(data: usize) -> Self {
        return Self {
            data: data,
            phantom: PhantomData,
        };
    }

    /// @brief 为新页表的页表项设置默认值
    /// 默认值为：
    /// - present
    /// - read only
    /// - kernel space
    /// - no exec
    #[inline(always)]
    pub fn new_page_table() -> Self {
        return unsafe {
            Self::from_data(
                Arch::ENTRY_FLAG_DEFAULT_TABLE
                    | Arch::ENTRY_FLAG_READONLY
                    | Arch::ENTRY_FLAG_NO_EXEC,
            )
        };
    }

    /// @brief 取得当前页表项的所有权，更新当前页表项的标志位，并返回更新后的页表项。
    ///
    /// @param flag 要更新的标志位的值
    /// @param value 如果为true，那么将flag对应的位设置为1，否则设置为0
    ///
    /// @return 更新后的页表项
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

    /// @brief 判断当前页表项是否存在指定的flag（只有全部flag都存在才返回true）
    #[inline(always)]
    pub fn has_flag(&self, flag: usize) -> bool {
        return self.data & flag == flag;
    }

    #[inline(always)]
    pub fn present(&self) -> bool {
        return self.has_flag(Arch::ENTRY_FLAG_PRESENT);
    }

    /// @brief 设置当前页表项的权限
    ///
    /// @param value 如果为true，那么将当前页表项的权限设置为用户态可访问
    #[must_use]
    #[inline(always)]
    pub fn set_user(self, value: bool) -> Self {
        return self.update_flags(Arch::ENTRY_FLAG_USER, value);
    }

    /// @brief 用户态是否可以访问当前页表项
    #[inline(always)]
    pub fn user(&self) -> bool {
        return self.has_flag(Arch::ENTRY_FLAG_USER);
    }

    /// @brief 设置当前页表项的可写性, 如果为true，那么将当前页表项的权限设置为可写, 否则设置为只读
    ///
    /// @return 更新后的页表项. 请注意，本函数会取得当前页表项的所有权，因此返回的页表项不是原来的页表项
    #[must_use]
    #[inline(always)]
    pub fn set_write(self, value: bool) -> Self {
        // 有的架构同时具有可写和不可写的标志位，因此需要同时更新
        return self
            .update_flags(Arch::ENTRY_FLAG_READONLY, !value)
            .update_flags(Arch::ENTRY_FLAG_READWRITE, value);
    }

    /// @brief 当前页表项是否可写
    #[inline(always)]
    pub fn write(&self) -> bool {
        // 有的架构同时具有可写和不可写的标志位，因此需要同时判断
        return self.data & (Arch::ENTRY_FLAG_READWRITE | Arch::ENTRY_FLAG_READONLY)
            == Arch::ENTRY_FLAG_READWRITE;
    }

    /// @brief 设置当前页表项的可执行性, 如果为true，那么将当前页表项的权限设置为可执行, 否则设置为不可执行
    #[must_use]
    #[inline(always)]
    pub fn set_execute(self, value: bool) -> Self {
        // 有的架构同时具有可执行和不可执行的标志位，因此需要同时更新
        return self
            .update_flags(Arch::ENTRY_FLAG_NO_EXEC, !value)
            .update_flags(Arch::ENTRY_FLAG_EXEC, value);
    }

    /// @brief 当前页表项是否可执行
    #[inline(always)]
    pub fn execute(&self) -> bool {
        // 有的架构同时具有可执行和不可执行的标志位，因此需要同时判断
        return self.data & (Arch::ENTRY_FLAG_EXEC | Arch::ENTRY_FLAG_NO_EXEC)
            == Arch::ENTRY_FLAG_EXEC;
    }
}

impl<Arch: MemoryManagementArch> fmt::Debug for PageFlags<Arch> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PageFlags")
            .field("bits", &format_args!("{:#0x}", self.data))
            .field("present", &self.present())
            .field("write", &self.write())
            .field("executable", &self.execute())
            .field("user", &self.user())
            .finish()
    }
}
