use alloc::string::ToString;
use core::{
    fmt::{self, Debug, Error, Formatter},
    marker::PhantomData,
    mem,
    ops::Add,
    sync::atomic::{compiler_fence, Ordering},
};
use system_error::SystemError;
use unified_init::macros::unified_init;

use alloc::sync::Arc;
use hashbrown::{HashMap, HashSet};
use log::{error, info};
use lru::LruCache;

use crate::{
    arch::{interrupt::ipi::send_ipi, mm::LockedFrameAllocator, MMArch},
    exception::ipi::{IpiKind, IpiTarget},
    filesystem::vfs::{file::PageCache, FilePrivateData},
    init::initcall::INITCALL_CORE,
    ipc::shm::ShmId,
    libs::{
        rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard},
        spinlock::{SpinLock, SpinLockGuard},
    },
    process::{ProcessControlBlock, ProcessManager},
    time::{sleep::usleep, PosixTimeSpec},
};

use super::{
    allocator::page_frame::{FrameAllocator, PageFrameCount},
    syscall::ProtFlags,
    ucontext::LockedVMA,
    MemoryManagementArch, PageTableKind, PhysAddr, VirtAddr,
};

pub const PAGE_4K_SHIFT: usize = 12;
#[allow(dead_code)]
pub const PAGE_2M_SHIFT: usize = 21;
pub const PAGE_1G_SHIFT: usize = 30;

pub const PAGE_4K_SIZE: usize = 1 << PAGE_4K_SHIFT;
pub const PAGE_2M_SIZE: usize = 1 << PAGE_2M_SHIFT;

/// 全局物理页信息管理器
pub static mut PAGE_MANAGER: Option<SpinLock<PageManager>> = None;

/// 初始化PAGE_MANAGER
pub fn page_manager_init() {
    info!("page_manager_init");
    let page_manager = SpinLock::new(PageManager::new());

    compiler_fence(Ordering::SeqCst);
    unsafe { PAGE_MANAGER = Some(page_manager) };
    compiler_fence(Ordering::SeqCst);

    info!("page_manager_init done");
}

pub fn page_manager_lock_irqsave() -> SpinLockGuard<'static, PageManager> {
    unsafe { PAGE_MANAGER.as_ref().unwrap().lock_irqsave() }
}

// 物理页管理器
pub struct PageManager {
    phys2page: HashMap<PhysAddr, Arc<Page>>,
}

impl PageManager {
    pub fn new() -> Self {
        Self {
            phys2page: HashMap::new(),
        }
    }

    pub fn contains(&self, paddr: &PhysAddr) -> bool {
        self.phys2page.contains_key(paddr)
    }

    pub fn get(&mut self, paddr: &PhysAddr) -> Option<Arc<Page>> {
        page_reclaimer_lock_irqsave().get(paddr);
        self.phys2page.get(paddr).cloned()
    }

    pub fn get_unwrap(&mut self, paddr: &PhysAddr) -> Arc<Page> {
        page_reclaimer_lock_irqsave().get(paddr);
        self.phys2page
            .get(paddr)
            .unwrap_or_else(|| panic!("Phys Page not found, {:?}", paddr))
            .clone()
    }

    pub fn insert(&mut self, paddr: PhysAddr, page: &Arc<Page>) {
        self.phys2page.insert(paddr, page.clone());
    }

    pub fn remove_page(&mut self, paddr: &PhysAddr) {
        self.phys2page.remove(paddr);
    }
}

pub static mut PAGE_RECLAIMER: Option<SpinLock<PageReclaimer>> = None;

pub fn page_reclaimer_init() {
    info!("page_reclaimer_init");
    let page_reclaimer = SpinLock::new(PageReclaimer::new());

    compiler_fence(Ordering::SeqCst);
    unsafe { PAGE_RECLAIMER = Some(page_reclaimer) };
    compiler_fence(Ordering::SeqCst);

    info!("page_reclaimer_init done");
}

/// 页面回收线程
static mut PAGE_RECLAIMER_THREAD: Option<Arc<ProcessControlBlock>> = None;

/// 页面回收线程初始化函数
#[unified_init(INITCALL_CORE)]
fn page_reclaimer_thread_init() -> Result<(), SystemError> {
    let closure = crate::process::kthread::KernelThreadClosure::StaticEmptyClosure((
        &(page_reclaim_thread as fn() -> i32),
        (),
    ));
    let pcb = crate::process::kthread::KernelThreadMechanism::create_and_run(
        closure,
        "page_reclaim".to_string(),
    )
    .ok_or("")
    .expect("create tty_refresh thread failed");
    unsafe {
        PAGE_RECLAIMER_THREAD = Some(pcb);
    }
    Ok(())
}

/// 页面回收线程执行的函数
fn page_reclaim_thread() -> i32 {
    loop {
        let usage = unsafe { LockedFrameAllocator.usage() };
        // log::info!("usage{:?}", usage);

        // 保留4096个页面，总计16MB的空闲空间
        if usage.free().data() < 4096 {
            let page_to_free = 4096;
            page_reclaimer_lock_irqsave().shrink_list(PageFrameCount::new(page_to_free));
        } else {
            //TODO 暂时让页面回收线程负责脏页回写任务，后续需要分离
            page_reclaimer_lock_irqsave().flush_dirty_pages();
            // 休眠5秒
            // log::info!("sleep");
            let _ = usleep(PosixTimeSpec::new(5, 0));
        }
    }
}

/// 获取页面回收器
pub fn page_reclaimer_lock_irqsave() -> SpinLockGuard<'static, PageReclaimer> {
    unsafe { PAGE_RECLAIMER.as_ref().unwrap().lock_irqsave() }
}

/// 页面回收器
pub struct PageReclaimer {
    lru: LruCache<PhysAddr, Arc<Page>>,
}

impl PageReclaimer {
    pub fn new() -> Self {
        Self {
            lru: LruCache::unbounded(),
        }
    }

    pub fn get(&mut self, paddr: &PhysAddr) -> Option<Arc<Page>> {
        self.lru.get(paddr).cloned()
    }

    pub fn insert_page(&mut self, paddr: PhysAddr, page: &Arc<Page>) {
        self.lru.put(paddr, page.clone());
    }

    /// lru链表缩减
    /// ## 参数
    ///
    /// - `count`: 需要缩减的页面数量
    pub fn shrink_list(&mut self, count: PageFrameCount) {
        for _ in 0..count.data() {
            let (paddr, page) = self.lru.pop_lru().expect("pagecache is empty");
            let page_cache = page.read_irqsave().page_cache().unwrap();
            for vma in page.read_irqsave().anon_vma() {
                let address_space = vma.lock_irqsave().address_space().unwrap();
                let address_space = address_space.upgrade().unwrap();
                let mut guard = address_space.write();
                let mapper = &mut guard.user_mapper.utable;
                let virt = vma.lock_irqsave().page_address(&page).unwrap();
                unsafe {
                    mapper.unmap(virt, false).unwrap().flush();
                }
            }
            page_cache.remove_page(page.read_irqsave().index().unwrap());
            page_manager_lock_irqsave().remove_page(&paddr);
            if page.read_irqsave().flags.contains(PageFlags::PG_DIRTY) {
                Self::page_writeback(&page, true);
            }
        }
    }

    /// 唤醒页面回收线程
    pub fn wakeup_claim_thread() {
        // log::info!("wakeup_claim_thread");
        let _ = ProcessManager::wakeup(unsafe { PAGE_RECLAIMER_THREAD.as_ref().unwrap() });
    }

    /// 脏页回写函数
    /// ## 参数
    ///
    /// - `page`: 需要回写的脏页
    /// - `unmap`: 是否取消映射
    ///
    /// ## 返回值
    /// - VmFaultReason: 页面错误处理信息标志
    pub fn page_writeback(page: &Arc<Page>, unmap: bool) {
        if !unmap {
            page.write_irqsave().remove_flags(PageFlags::PG_DIRTY);
        }

        for vma in page.read_irqsave().anon_vma() {
            let address_space = vma.lock_irqsave().address_space().unwrap();
            let address_space = address_space.upgrade().unwrap();
            let mut guard = address_space.write();
            let mapper = &mut guard.user_mapper.utable;
            let virt = vma.lock_irqsave().page_address(page).unwrap();
            if unmap {
                unsafe {
                    mapper.unmap(virt, false).unwrap().flush();
                }
            } else {
                unsafe {
                    // 保护位设为只读
                    mapper.remap(
                        virt,
                        mapper.get_entry(virt, 0).unwrap().flags().set_write(false),
                    )
                };
            }
        }
        let inode = page
            .read_irqsave()
            .page_cache
            .clone()
            .unwrap()
            .inode()
            .clone()
            .unwrap()
            .upgrade()
            .unwrap();
        inode
            .write_at(
                page.read_irqsave().index().unwrap(),
                MMArch::PAGE_SIZE,
                unsafe {
                    core::slice::from_raw_parts(
                        MMArch::phys_2_virt(page.read_irqsave().phys_addr)
                            .unwrap()
                            .data() as *mut u8,
                        MMArch::PAGE_SIZE,
                    )
                },
                SpinLock::new(FilePrivateData::Unused).lock(),
            )
            .unwrap();
    }

    /// lru脏页刷新
    pub fn flush_dirty_pages(&self) {
        // log::info!("flush_dirty_pages");
        let iter = self.lru.iter();
        for (_, page) in iter {
            if page.read_irqsave().flags().contains(PageFlags::PG_DIRTY) {
                Self::page_writeback(page, false);
            }
        }
    }
}

bitflags! {
    pub struct PageFlags: u64 {
        const PG_LOCKED = 1 << 0;
        const PG_WRITEBACK = 1 << 1;
        const PG_REFERENCED = 1 << 2;
        const PG_UPTODATE = 1 << 3;
        const PG_DIRTY = 1 << 4;
        const PG_LRU = 1 << 5;
        const PG_HEAD = 1 << 6;
        const PG_WAITERS = 1 << 7;
        const PG_ACTIVE = 1 << 8;
        const PG_WORKINGSET = 1 << 9;
        const PG_ERROR = 1 << 10;
        const PG_SLAB = 1 << 11;
        const PG_RESERVED = 1 << 14;
        const PG_PRIVATE = 1 << 15;
        const PG_RECLAIM = 1 << 18;
        const PG_SWAPBACKED = 1 << 19;
    }
}

#[derive(Debug)]
pub struct Page {
    inner: RwLock<InnerPage>,
}

impl Page {
    pub fn new(shared: bool, phys_addr: PhysAddr) -> Self {
        let inner = InnerPage::new(shared, phys_addr);
        Self {
            inner: RwLock::new(inner),
        }
    }

    pub fn read_irqsave(&self) -> RwLockReadGuard<InnerPage> {
        self.inner.read_irqsave()
    }

    pub fn write_irqsave(&self) -> RwLockWriteGuard<InnerPage> {
        self.inner.write_irqsave()
    }
}

#[derive(Debug)]
/// 物理页面信息
pub struct InnerPage {
    /// 映射计数
    map_count: usize,
    /// 是否为共享页
    shared: bool,
    /// 映射计数为0时，是否可回收
    free_when_zero: bool,
    /// 共享页id（如果是共享页）
    shm_id: Option<ShmId>,
    /// 映射到当前page的VMA
    anon_vma: HashSet<Arc<LockedVMA>>,
    /// 标志
    flags: PageFlags,
    /// 页所在的物理页帧号
    phys_addr: PhysAddr,
    /// 在pagecache中的偏移
    index: Option<usize>,
    page_cache: Option<Arc<PageCache>>,
}

impl InnerPage {
    pub fn new(shared: bool, phys_addr: PhysAddr) -> Self {
        let dealloc_when_zero = !shared;
        Self {
            map_count: 0,
            shared,
            free_when_zero: dealloc_when_zero,
            shm_id: None,
            anon_vma: HashSet::new(),
            flags: PageFlags::empty(),
            phys_addr,
            index: None,
            page_cache: None,
        }
    }

    /// 将vma加入anon_vma
    pub fn insert_vma(&mut self, vma: Arc<LockedVMA>) {
        self.anon_vma.insert(vma);
        self.map_count += 1;
    }

    /// 将vma从anon_vma中删去
    pub fn remove_vma(&mut self, vma: &LockedVMA) {
        self.anon_vma.remove(vma);
        self.map_count -= 1;
    }

    /// 判断当前物理页是否能被回
    pub fn can_deallocate(&self) -> bool {
        self.map_count == 0 && self.free_when_zero
    }

    pub fn shared(&self) -> bool {
        self.shared
    }

    pub fn shm_id(&self) -> Option<ShmId> {
        self.shm_id
    }

    pub fn index(&self) -> Option<usize> {
        self.index
    }

    pub fn page_cache(&self) -> Option<Arc<PageCache>> {
        self.page_cache.clone()
    }

    pub fn set_page_cache(&mut self, page_cache: Option<Arc<PageCache>>) {
        self.page_cache = page_cache;
    }

    pub fn set_index(&mut self, index: Option<usize>) {
        self.index = index;
    }

    pub fn set_page_cache_index(
        &mut self,
        page_cache: Option<Arc<PageCache>>,
        index: Option<usize>,
    ) {
        self.page_cache = page_cache;
        self.index = index;
    }

    pub fn set_shm_id(&mut self, shm_id: ShmId) {
        self.shm_id = Some(shm_id);
    }

    pub fn set_dealloc_when_zero(&mut self, dealloc_when_zero: bool) {
        self.free_when_zero = dealloc_when_zero;
    }

    #[inline(always)]
    pub fn anon_vma(&self) -> &HashSet<Arc<LockedVMA>> {
        &self.anon_vma
    }

    #[inline(always)]
    pub fn map_count(&self) -> usize {
        self.map_count
    }

    #[inline(always)]
    pub fn flags(&self) -> &PageFlags {
        &self.flags
    }

    #[inline(always)]
    pub fn set_flags(&mut self, flags: PageFlags) {
        self.flags = flags
    }

    #[inline(always)]
    pub fn add_flags(&mut self, flags: PageFlags) {
        self.flags = self.flags.union(flags);
    }

    #[inline(always)]
    pub fn remove_flags(&mut self, flags: PageFlags) {
        self.flags = self.flags.difference(flags);
    }

    #[inline(always)]
    pub fn phys_address(&self) -> PhysAddr {
        self.phys_addr
    }
}

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
    pub fn index_of(&self, addr: VirtAddr) -> Option<usize> {
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

    /// 拷贝页表
    /// ## 参数
    ///
    /// - `allocator`: 物理页框分配器
    /// - `copy_on_write`: 是否写时复制
    pub unsafe fn clone(
        &self,
        allocator: &mut impl FrameAllocator,
        copy_on_write: bool,
    ) -> Option<PageTable<Arch>> {
        // 分配新页面作为新的页表
        let phys = allocator.allocate_one()?;
        let frame = MMArch::phys_2_virt(phys).unwrap();
        MMArch::write_bytes(frame, 0, MMArch::PAGE_SIZE);
        let new_table = PageTable::new(self.base, phys, self.level);
        if self.level == 0 {
            for i in 0..Arch::PAGE_ENTRY_NUM {
                if let Some(mut entry) = self.entry(i) {
                    if entry.present() {
                        if copy_on_write {
                            let mut new_flags = entry.flags().set_write(false);
                            entry.set_flags(new_flags);
                            self.set_entry(i, entry);
                            new_flags = new_flags.set_dirty(false);
                            entry.set_flags(new_flags);
                            new_table.set_entry(i, entry);
                        } else {
                            let phys = allocator.allocate_one()?;
                            let mut page_manager_guard = page_manager_lock_irqsave();
                            let old_phys = entry.address().unwrap();
                            let old_page = page_manager_guard.get_unwrap(&old_phys);
                            let new_page =
                                Arc::new(Page::new(old_page.read_irqsave().shared(), phys));
                            if let Some(ref page_cache) = old_page.read_irqsave().page_cache() {
                                new_page.write_irqsave().set_page_cache_index(
                                    Some(page_cache.clone()),
                                    old_page.read_irqsave().index(),
                                );
                            }

                            page_manager_guard.insert(phys, &new_page);
                            let old_phys = entry.address().unwrap();
                            let frame = MMArch::phys_2_virt(phys).unwrap().data() as *mut u8;
                            frame.copy_from_nonoverlapping(
                                MMArch::phys_2_virt(old_phys).unwrap().data() as *mut u8,
                                MMArch::PAGE_SIZE,
                            );
                            new_table.set_entry(i, PageEntry::new(phys, entry.flags()));
                        }
                    }
                }
            }
        } else {
            // 非一级页表拷贝时，对每个页表项对应的页表都进行拷贝
            for i in 0..MMArch::PAGE_ENTRY_NUM {
                if let Some(next_table) = self.next_level_table(i) {
                    let table = next_table.clone(allocator, copy_on_write)?;
                    let old_entry = self.entry(i).unwrap();
                    let entry = PageEntry::new(table.phys(), old_entry.flags());
                    new_table.set_entry(i, entry);
                }
            }
        }
        Some(new_table)
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
    pub fn new(paddr: PhysAddr, flags: EntryFlags<Arch>) -> Self {
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
    pub fn flags(&self) -> EntryFlags<Arch> {
        unsafe { EntryFlags::from_data(self.data & Arch::ENTRY_FLAGS_MASK) }
    }

    #[inline(always)]
    pub fn set_flags(&mut self, flags: EntryFlags<Arch>) {
        self.data = (self.data & !Arch::ENTRY_FLAGS_MASK) | flags.data();
    }

    #[inline(always)]
    pub fn present(&self) -> bool {
        return self.data & Arch::ENTRY_FLAG_PRESENT != 0;
    }

    #[inline(always)]
    pub fn empty(&self) -> bool {
        self.data & !(Arch::ENTRY_FLAG_DIRTY & Arch::ENTRY_FLAG_ACCESSED) == 0
    }

    #[inline(always)]
    pub fn protnone(&self) -> bool {
        return self.data & (Arch::ENTRY_FLAG_PRESENT | Arch::ENTRY_FLAG_GLOBAL)
            == Arch::ENTRY_FLAG_GLOBAL;
    }

    #[inline(always)]
    pub fn write(&self) -> bool {
        return self.data & Arch::ENTRY_FLAG_READWRITE != 0;
    }
}

/// 页表项的标志位
#[derive(Copy, Clone, Hash)]
pub struct EntryFlags<Arch> {
    data: usize,
    phantom: PhantomData<Arch>,
}

impl<Arch: MemoryManagementArch> Default for EntryFlags<Arch> {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(dead_code)]
impl<Arch: MemoryManagementArch> EntryFlags<Arch> {
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

    /// 根据ProtFlags生成EntryFlags
    ///
    /// ## 参数
    ///
    /// - prot_flags: 页的保护标志
    /// - user: 用户空间是否可访问
    pub fn from_prot_flags(prot_flags: ProtFlags, user: bool) -> Self {
        let vm_flags = super::VmFlags::from(prot_flags);
        // let flags: EntryFlags<Arch> = EntryFlags::new()
        //     .set_user(user)
        //     .set_execute(prot_flags.contains(ProtFlags::PROT_EXEC))
        //     .set_write(prot_flags.contains(ProtFlags::PROT_WRITE));
        let flags = Arch::vm_get_page_prot(vm_flags).set_user(user);
        return flags;
    }

    #[inline(always)]
    pub fn data(&self) -> usize {
        self.data
    }

    #[inline(always)]
    pub const unsafe fn from_data(data: usize) -> Self {
        return Self {
            data,
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

            #[cfg(target_arch = "x86_64")]
            {
                if user {
                    r.set_user(true)
                } else {
                    r
                }
            }

            #[cfg(target_arch = "riscv64")]
            {
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
                return self
                    .update_flags(Arch::ENTRY_FLAG_READONLY, true)
                    .update_flags(Arch::ENTRY_FLAG_WRITEABLE, false);
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

    #[inline(always)]
    pub fn set_page_global(self, value: bool) -> Self {
        return self.update_flags(MMArch::ENTRY_FLAG_GLOBAL, value);
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

    /// 设置当前页表是否为脏页
    ///
    /// ## 参数
    ///
    /// - value: 如果为true，那么将当前页表项的写穿策略设置为写穿。
    #[inline(always)]
    pub fn set_dirty(self, value: bool) -> Self {
        return self.update_flags(Arch::ENTRY_FLAG_DIRTY, value);
    }

    /// 设置当前页表被访问
    ///
    /// ## 参数
    ///
    /// - value: 如果为true，那么将当前页表项的访问标志设置为已访问。
    #[inline(always)]
    pub fn set_access(self, value: bool) -> Self {
        return self.update_flags(Arch::ENTRY_FLAG_ACCESSED, value);
    }

    /// 设置指向的页是否为大页
    ///
    /// ## 参数
    ///
    /// - value: 如果为true，那么将当前页表项的访问标志设置为已访问。
    #[inline(always)]
    pub fn set_huge_page(self, value: bool) -> Self {
        return self.update_flags(Arch::ENTRY_FLAG_HUGE_PAGE, value);
    }

    /// MMIO内存的页表项标志
    #[inline(always)]
    pub fn mmio_flags() -> Self {
        #[cfg(target_arch = "x86_64")]
        {
            Self::new()
                .set_user(false)
                .set_write(true)
                .set_execute(true)
                .set_page_cache_disable(true)
                .set_page_write_through(true)
                .set_page_global(true)
        }

        #[cfg(target_arch = "riscv64")]
        {
            Self::new()
                .set_user(false)
                .set_write(true)
                .set_execute(true)
                .set_page_global(true)
        }
    }
}

impl<Arch: MemoryManagementArch> fmt::Debug for EntryFlags<Arch> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EntryFlags")
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
        flags: EntryFlags<Arch>,
    ) -> Option<PageFlush<Arch>> {
        compiler_fence(Ordering::SeqCst);
        let phys: PhysAddr = self.frame_allocator.allocate_one()?;
        compiler_fence(Ordering::SeqCst);

        unsafe {
            let vaddr = MMArch::phys_2_virt(phys).unwrap();
            MMArch::write_bytes(vaddr, 0, MMArch::PAGE_SIZE);
        }

        let mut page_manager_guard: SpinLockGuard<'static, PageManager> =
            page_manager_lock_irqsave();
        if !page_manager_guard.contains(&phys) {
            page_manager_guard.insert(phys, &Arc::new(Page::new(false, phys)))
        }
        drop(page_manager_guard);
        return self.map_phys(virt, phys, flags);
    }

    /// 映射一个物理页到指定的虚拟地址
    pub unsafe fn map_phys(
        &mut self,
        virt: VirtAddr,
        phys: PhysAddr,
        flags: EntryFlags<Arch>,
    ) -> Option<PageFlush<Arch>> {
        // 验证虚拟地址和物理地址是否对齐
        if !(virt.check_aligned(Arch::PAGE_SIZE) && phys.check_aligned(Arch::PAGE_SIZE)) {
            error!(
                "Try to map unaligned page: virt={:?}, phys={:?}",
                virt, phys
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
                compiler_fence(Ordering::SeqCst);

                table.set_entry(i, entry);
                compiler_fence(Ordering::SeqCst);
                return Some(PageFlush::new(virt));
            } else {
                let next_table = table.next_level_table(i);
                if let Some(next_table) = next_table {
                    table = next_table;
                    // debug!("Mapping {:?} to next level table...", virt);
                } else {
                    // 分配下一级页表
                    let frame = self.frame_allocator.allocate_one()?;

                    // 清空这个页帧
                    MMArch::write_bytes(MMArch::phys_2_virt(frame).unwrap(), 0, MMArch::PAGE_SIZE);
                    // 设置页表项的flags
                    let flags: EntryFlags<Arch> =
                        EntryFlags::new_page_table(virt.kind() == PageTableKind::User);

                    // 把新分配的页表映射到当前页表
                    table.set_entry(i, PageEntry::new(frame, flags));

                    // 获取新分配的页表
                    table = table.next_level_table(i)?;
                }
            }
        }
    }

    /// 进行大页映射
    pub unsafe fn map_huge_page(
        &mut self,
        virt: VirtAddr,
        flags: EntryFlags<Arch>,
    ) -> Option<PageFlush<Arch>> {
        // 验证虚拟地址是否对齐
        if !(virt.check_aligned(Arch::PAGE_SIZE)) {
            error!("Try to map unaligned page: virt={:?}", virt);
            return None;
        }

        let virt = VirtAddr::new(virt.data() & (!Arch::PAGE_NEGATIVE_MASK));

        let mut table = self.table();
        loop {
            let i = table.index_of(virt)?;
            assert!(i < Arch::PAGE_ENTRY_NUM);
            let next_table = table.next_level_table(i);
            if let Some(next_table) = next_table {
                table = next_table;
            } else {
                break;
            }
        }

        // 支持2M、1G大页，即页表层级为1、2级的页表可以映射大页
        if table.level == 0 || table.level > 2 {
            return None;
        }

        let (phys, count) = self.frame_allocator.allocate(PageFrameCount::new(
            Arch::PAGE_ENTRY_NUM.pow(table.level as u32),
        ))?;

        MMArch::write_bytes(
            MMArch::phys_2_virt(phys).unwrap(),
            0,
            MMArch::PAGE_SIZE * count.data(),
        );

        table.set_entry(
            table.index_of(virt)?,
            PageEntry::new(phys, flags.set_huge_page(true)),
        )?;
        Some(PageFlush::new(virt))
    }

    /// 为虚拟地址分配指定层级的页表
    /// ## 参数
    ///
    /// - `virt`: 虚拟地址
    /// - `level`: 指定页表层级
    ///
    /// ## 返回值
    /// - Some(PageTable<Arch>): 虚拟地址对应层级的页表
    /// - None: 对应页表不存在
    pub unsafe fn allocate_table(
        &mut self,
        virt: VirtAddr,
        level: usize,
    ) -> Option<PageTable<Arch>> {
        let table = self.get_table(virt, level + 1)?;
        let i = table.index_of(virt)?;
        let frame = self.frame_allocator.allocate_one()?;

        // 清空这个页帧
        MMArch::write_bytes(MMArch::phys_2_virt(frame).unwrap(), 0, MMArch::PAGE_SIZE);

        // 设置页表项的flags
        let flags: EntryFlags<Arch> =
            EntryFlags::new_page_table(virt.kind() == PageTableKind::User);

        table.set_entry(i, PageEntry::new(frame, flags));
        table.next_level_table(i)
    }

    /// 获取虚拟地址的指定层级页表
    /// ## 参数
    ///
    /// - `virt`: 虚拟地址
    /// - `level`: 指定页表层级
    ///
    /// ## 返回值
    /// - Some(PageTable<Arch>): 虚拟地址对应层级的页表
    /// - None: 对应页表不存在
    pub fn get_table(&self, virt: VirtAddr, level: usize) -> Option<PageTable<Arch>> {
        let mut table = self.table();
        if level > Arch::PAGE_LEVELS - 1 {
            return None;
        }

        unsafe {
            loop {
                if table.level == level {
                    return Some(table);
                }
                let i = table.index_of(virt)?;
                assert!(i < Arch::PAGE_ENTRY_NUM);

                table = table.next_level_table(i)?;
            }
        }
    }

    /// 获取虚拟地址在指定层级页表的PageEntry
    /// ## 参数
    ///
    /// - `virt`: 虚拟地址
    /// - `level`: 指定页表层级
    ///
    /// ## 返回值
    /// - Some(PageEntry<Arch>): 虚拟地址在指定层级的页表的有效PageEntry
    /// - None: 无对应的有效PageEntry
    pub fn get_entry(&self, virt: VirtAddr, level: usize) -> Option<PageEntry<Arch>> {
        let table = self.get_table(virt, level)?;
        let i = table.index_of(virt)?;
        let entry = unsafe { table.entry(i) }?;

        if !entry.empty() {
            Some(entry)
        } else {
            None
        }

        // let mut table = self.table();
        // if level > Arch::PAGE_LEVELS - 1 {
        //     return None;
        // }
        // unsafe {
        //     loop {
        //         let i = table.index_of(virt)?;
        //         assert!(i < Arch::PAGE_ENTRY_NUM);

        //         if table.level == level {
        //             let entry = table.entry(i)?;
        //             if !entry.empty() {
        //                 return Some(entry);
        //             } else {
        //                 return None;
        //             }
        //         }

        //         table = table.next_level_table(i)?;
        //     }
        // }
    }

    /// 拷贝用户空间映射
    /// ## 参数
    ///
    /// - `umapper`: 要拷贝的用户空间
    /// - `copy_on_write`: 是否写时复制
    pub unsafe fn clone_user_mapping(&mut self, umapper: &mut Self, copy_on_write: bool) {
        let old_table = umapper.table();
        let new_table = self.table();
        let allocator = self.allocator_mut();
        // 顶级页表的[0, PAGE_KERNEL_INDEX)项为用户空间映射
        for entry_index in 0..Arch::PAGE_KERNEL_INDEX {
            if let Some(next_table) = old_table.next_level_table(entry_index) {
                let table = next_table.clone(allocator, copy_on_write).unwrap();
                let old_entry = old_table.entry(entry_index).unwrap();
                let entry = PageEntry::new(table.phys(), old_entry.flags());
                new_table.set_entry(entry_index, entry);
            }
        }
    }

    /// 将物理地址映射到具有线性偏移量的虚拟地址
    #[allow(dead_code)]
    pub unsafe fn map_linearly(
        &mut self,
        phys: PhysAddr,
        flags: EntryFlags<Arch>,
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
        flags: EntryFlags<Arch>,
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
    pub fn translate(&self, virt: VirtAddr) -> Option<(PhysAddr, EntryFlags<Arch>)> {
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
    ) -> Option<(PhysAddr, EntryFlags<Arch>, PageFlush<Arch>)> {
        if !virt.check_aligned(Arch::PAGE_SIZE) {
            error!("Try to unmap unaligned page: virt={:?}", virt);
            return None;
        }

        let table = self.table();
        return unmap_phys_inner(virt, &table, unmap_parents, self.allocator_mut())
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
) -> Option<(PhysAddr, EntryFlags<Arch>)> {
    // 获取页表项的索引
    let i = table.index_of(vaddr)?;

    // 如果当前是最后一级页表，直接取消页面映射
    if table.level() == 0 {
        let entry = table.entry(i)?;
        table.set_entry(i, PageEntry::from_usize(0));
        return Some((entry.address().ok()?, entry.flags()));
    }

    let subtable = table.next_level_table(i)?;
    // 递归地取消映射
    let result = unmap_phys_inner(vaddr, &subtable, unmap_parents, allocator)?;

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
