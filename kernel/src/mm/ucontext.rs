// 进程的用户空间内存管理

use core::{cmp, hash::Hasher, intrinsics::unlikely, ops::Add};

use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
};
use hashbrown::HashSet;

use crate::{
    arch::{mm::PageMapper, MMArch},
    libs::spinlock::{SpinLock, SpinLockGuard},
    syscall::SystemError,
};

use super::{
    allocator::page_frame::{deallocate_page_frames, PageFrameCount, PhysPageFrame, VirtPageFrame},
    page::{Flusher, PageFlags},
    syscall::MapFlags,
    MemoryManagementArch, PageTableKind, VirtAddr, VirtRegion,
};

/// @brief 用户地址空间结构体（每个进程都有一个）
#[derive(Debug)]
pub struct AddressSpace {
    pub user_mapper: UserMapper,
    pub mappings: UserMappings,
    pub mmap_min: usize,
}

#[derive(Debug, Hash)]
pub struct UserMapper {
    pub utable: PageMapper,
}

impl UserMapper {
    pub fn new(utable: PageMapper) -> Self {
        return Self { utable };
    }
}

impl Drop for UserMapper {
    fn drop(&mut self) {
        if self.utable.is_current() {
            // 如果当前要被销毁的用户空间的页表是当前进程的页表，那么就切换回初始内核页表
            unsafe { MMArch::set_table(PageTableKind::User, MMArch::initial_page_table()) }
        }
        // 释放用户空间顶层页表占用的页帧
        // 请注意，在释放这个页帧之前，用户页表应该已经被完全释放，否则会产生内存泄露
        deallocate_page_frames(
            PhysPageFrame::new(self.utable.table().phys()),
            PageFrameCount::new(1),
        );
    }
}

/// 用户空间映射信息
#[derive(Debug)]
pub struct UserMappings {
    /// 当前用户空间的虚拟内存区域
    vmas: HashSet<Arc<LockedVMA>>,
    /// 当前用户空间的VMA空洞
    vm_holes: BTreeMap<VirtAddr, usize>,
}

impl UserMappings {
    pub fn new() -> Self {
        return Self {
            vmas: HashSet::new(),
            vm_holes: BTreeMap::new(),
        };
    }

    /// 判断当前进程的VMA内，是否有包含指定的虚拟地址的VMA。
    ///
    /// 如果有，返回包含指定虚拟地址的VMA的Arc指针，否则返回None。
    pub fn contains(&self, vaddr: VirtAddr) -> Option<Arc<LockedVMA>> {
        for v in self.vmas.iter() {
            let guard = v.lock();
            if guard.region.contains(vaddr) {
                return Some(v.clone());
            }
        }
        return None;
    }

    /// 获取当前进程的地址空间中，与给定虚拟地址范围有重叠的VMA的迭代器。
    pub fn conflicts(&self, request: VirtRegion) -> impl Iterator<Item = Arc<LockedVMA>> + '_ {
        let r = self
            .vmas
            .iter()
            .filter(move |v| !v.lock().region.intersect(&request).is_none())
            .cloned();
        return r;
    }

    /// 在当前进程的地址空间中，寻找第一个符合条件的空闲的虚拟内存范围。
    ///
    /// @param min_vaddr 最小的起始地址
    /// @param size 请求的大小
    ///
    /// @return 如果找到了，返回虚拟内存范围，否则返回None
    pub fn find_free(&self, min_vaddr: VirtAddr, size: usize) -> Option<VirtRegion> {
        let mut vaddr = min_vaddr;
        let mut iter = self
            .vm_holes
            .iter()
            .skip_while(|(hole_vaddr, hole_size)| hole_vaddr.add(**hole_size) <= min_vaddr);

        let (hole_vaddr, size) = iter.find(|(hole_vaddr, hole_size)| {
            // 计算当前空洞的可用大小
            let available_size: usize =
                if hole_vaddr <= &&min_vaddr && min_vaddr <= hole_vaddr.add(**hole_size) {
                    **hole_size - (min_vaddr - **hole_vaddr).data()
                } else {
                    **hole_size
                };

            size <= available_size
        })?;

        // 创建一个新的虚拟内存范围。
        let region = VirtRegion::new(cmp::max(*hole_vaddr, min_vaddr), *size);
        return Some(region);
    }

    pub fn find_free_at(
        &self,
        min_vaddr: VirtAddr,
        vaddr: VirtAddr,
        size: usize,
        flags: MapFlags,
    ) -> Result<VirtRegion, SystemError> {
        // 如果没有指定地址，那么就在当前进程的地址空间中寻找一个空闲的虚拟内存范围。
        if vaddr == VirtAddr::new(0) {
            return self.find_free(min_vaddr, size).ok_or(SystemError::ENOMEM);
        }

        // 如果指定了地址，那么就检查指定的地址是否可用。

        let requested = VirtRegion::new(vaddr, size);

        if requested.end() >= MMArch::USER_END_VADDR || !vaddr.check_aligned(MMArch::PAGE_SIZE) {
            return Err(SystemError::EINVAL);
        }

        if let Some(_x) = self.conflicts(requested).next() {
            if flags.contains(MapFlags::MAP_FIXED_NOREPLACE) {
                return Err(SystemError::EEXIST);
            }

            if flags.contains(MapFlags::MAP_FIXED) {
                // 如果指定了MAP_FIXED标志，由于所指定的地址无法成功建立映射，则放弃映射，不对地址做修正
                return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
            }

            // 如果没有指定MAP_FIXED标志，那么就对地址做修正
            let requested = self.find_free(min_vaddr, size).ok_or(SystemError::ENOMEM)?;
            return Ok(requested);
        }

        return Ok(requested);
    }

    /// 在当前进程的地址空间中，保留一个指定大小的区域，使得该区域不在空洞中。
    /// 该函数会修改vm_holes中的空洞信息。
    ///
    /// @param region 要保留的区域
    ///
    /// 请注意，在调用本函数之前，必须先确定region所在范围内没有VMA。
    fn reserve_hole(&mut self, region: &VirtRegion) {
        let prev_hole: Option<(&VirtAddr, &mut usize)> =
            self.vm_holes.range_mut(..region.start()).next_back();

        if let Some((prev_hole_vaddr, prev_hole_size)) = prev_hole {
            let prev_hole_end = prev_hole_vaddr.add(*prev_hole_size);

            if prev_hole_end > region.start() {
                // 如果前一个空洞的结束地址大于当前空洞的起始地址，那么就需要调整前一个空洞的大小。
                *prev_hole_size = region.start().data() - prev_hole_vaddr.data();
            }

            if prev_hole_end > region.end() {
                // 如果前一个空洞的结束地址大于当前空洞的结束地址，那么就需要增加一个新的空洞。
                self.vm_holes
                    .insert(region.end(), (prev_hole_end - region.end()).data());
            }
        }
    }

    /// 在当前进程的地址空间中，释放一个指定大小的区域，使得该区域成为一个空洞。
    /// 该函数会修改vm_holes中的空洞信息。
    fn unreserve_hole(&mut self, region: &VirtRegion) {
        // 如果将要插入的空洞与后一个空洞相邻，那么就需要合并。
        let next_hole_size: Option<usize> = self.vm_holes.remove(&region.end());

        if let Some((prev_hole_vaddr, prev_hole_size)) = self
            .vm_holes
            .range_mut(..region.start())
            .next_back()
            .filter(|(offset, size)| offset.data() + **size == region.start().data())
        {
            *prev_hole_size += region.size() + next_hole_size.unwrap_or(0);
        } else {
            self.vm_holes
                .insert(region.start(), region.size() + next_hole_size.unwrap_or(0));
        }
    }

    /// 在当前进程的映射关系中，插入一个新的VMA。
    pub fn insert_vma(&mut self, vma: Arc<LockedVMA>) {
        let region = vma.lock().region.clone();
        // 要求插入的地址范围必须是空闲的，也就是说，当前进程的地址空间中，不能有任何与之重叠的VMA。
        assert!(self.conflicts(region).next().is_none());
        self.reserve_hole(&region);

        self.vmas.insert(vma);
    }

    /// @brief 删除一个VMA，并把对应的地址空间加入空洞中。
    ///
    /// 这里不会取消VMA对应的地址的映射
    ///
    /// @param region 要删除的VMA所在的地址范围
    ///
    /// @return 如果成功删除了VMA，则返回被删除的VMA，否则返回None
    /// 如果没有可以删除的VMA，则不会执行删除操作，并报告失败。
    pub fn remove_vma(&mut self, region: &VirtRegion) -> Option<Arc<LockedVMA>> {
        // 请注意，由于这里会对每个VMA加锁，因此性能很低
        let vma: Arc<LockedVMA> = self
            .vmas
            .drain_filter(|vma| vma.lock().region == *region)
            .next()?;
        self.unreserve_hole(region);

        return Some(vma);
    }

    /// @brief Get the iterator of all VMAs in this process.
    pub fn iter_vmas(&self) -> hashbrown::hash_set::Iter<Arc<LockedVMA>> {
        return self.vmas.iter();
    }
}

impl Default for UserMappings {
    fn default() -> Self {
        return Self::new();
    }
}

/// 加了锁的VMA
///
/// 备注：进行性能测试，看看SpinLock和RwLock哪个更快。
#[derive(Debug)]
pub struct LockedVMA(SpinLock<VMA>);

impl core::hash::Hash for LockedVMA {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.lock().hash(state);
    }
}

impl PartialEq for LockedVMA {
    fn eq(&self, other: &Self) -> bool {
        self.0.lock().eq(&other.0.lock())
    }
}

impl Eq for LockedVMA {}

impl LockedVMA {
    pub fn new(vma: VMA) -> Arc<Self> {
        let r = Arc::new(Self(SpinLock::new(vma)));
        r.0.lock().self_ref = Arc::downgrade(&r);
        return r;
    }

    pub fn lock(&self) -> SpinLockGuard<VMA> {
        return self.0.lock();
    }

    /// 调整当前VMA的页面的标志位
    ///
    /// TODO：增加调整虚拟页映射的物理地址的功能
    ///
    /// @param flags 新的标志位
    /// @param mapper 页表映射器
    /// @param flusher 页表项刷新器
    ///
    pub fn remap(
        &self,
        flags: PageFlags<MMArch>,
        mapper: &mut PageMapper,
        mut flusher: impl Flusher<MMArch>,
    ) -> Result<(), SystemError> {
        let mut guard = self.lock();
        assert!(guard.mapped);
        for page in guard.region.pages() {
            // 暂时要求所有的页帧都已经映射到页表
            // TODO: 引入Lazy Mapping, 通过缺页中断来映射页帧，这里就不必要求所有的页帧都已经映射到页表了
            let r = unsafe {
                mapper
                    .remap(page.virt_address(), flags)
                    .expect("Failed to remap, beacuse of some page is not mapped")
            };
            flusher.consume(r);
        }
        guard.flags = flags;
        return Ok(());
    }

    pub fn unmap(&self, mapper: &mut PageMapper, mut flusher: impl Flusher<MMArch>) {
        let mut guard = self.lock();
        assert!(guard.mapped);
        for page in guard.region.pages() {
            let (paddr, _, flush) = unsafe { mapper.unmap_phys(page.virt_address(), true) }
                .expect("Failed to unmap, beacuse of some page is not mapped");

            // todo: 获取物理页的anon_vma的守卫

            // todo: 从anon_vma中删除当前VMA

            // todo: 如果物理页的anon_vma链表长度为0，则释放物理页.

            // 目前由于还没有实现共享页，所以直接释放物理页也没问题。
            // 但是在实现共享页之后，就不能直接释放物理页了，需要在anon_vma链表长度为0的时候才能释放物理页
            deallocate_page_frames(PhysPageFrame::new(paddr), PageFrameCount::new(1));

            flusher.consume(flush);
        }
        guard.mapped = false;
    }

    pub fn mapped(&self) -> bool {
        return self.0.lock().mapped;
    }

    /// 将当前VMA进行切分，切分成3个VMA，分别是：
    ///
    /// 1. 前面的VMA，如果没有则为None
    /// 2. 中间的VMA，也就是传入的Region
    /// 3. 后面的VMA，如果没有则为None
    pub fn extract(
        &self,
        region: VirtRegion,
    ) -> Option<(
        Option<Arc<LockedVMA>>,
        Arc<LockedVMA>,
        Option<Arc<LockedVMA>>,
    )> {
        assert!(region.start().check_aligned(MMArch::PAGE_SIZE));
        assert!(region.end().check_aligned(MMArch::PAGE_SIZE));

        let mut guard = self.lock();
        {
            // 如果传入的region不在当前VMA的范围内，则直接返回None
            if unlikely(region.start() < guard.region.start() || region.end() > guard.region.end())
            {
                return None;
            }

            let intersect: Option<VirtRegion> = guard.region.intersect(&region);
            // 如果当前VMA不包含region，则直接返回None
            if unlikely(intersect.is_none()) {
                return None;
            }
            let intersect: VirtRegion = intersect.unwrap();
            if unlikely(intersect == guard.region) {
                // 如果当前VMA完全包含region，则直接返回当前VMA
                return Some((None, guard.self_ref.upgrade().unwrap(), None));
            }
        }

        let before: Option<Arc<LockedVMA>> = guard.region.before(&region).map(|virt_region| {
            let mut vma: VMA = unsafe { guard.clone() };
            vma.region = virt_region;

            let vma: Arc<LockedVMA> = LockedVMA::new(vma);
            vma
        });

        let after: Option<Arc<LockedVMA>> = guard.region.after(&region).map(|virt_region| {
            let mut vma: VMA = unsafe { guard.clone() };
            vma.region = virt_region;

            let vma: Arc<LockedVMA> = LockedVMA::new(vma);
            vma
        });

        guard.region = region;

        // TODO: 重新设置before、after这两个VMA里面的物理页的anon_vma

        return Some((before, guard.self_ref.upgrade().unwrap(), after));
    }
}

/// @brief 虚拟内存区域
#[derive(Debug)]
pub struct VMA {
    /// 虚拟内存区域对应的虚拟地址范围
    region: VirtRegion,
    /// VMA内的页帧的标志
    flags: PageFlags<MMArch>,
    /// VMA内的页帧是否已经映射到页表
    mapped: bool,
    /// VMA所属的用户地址空间
    user_address_space: Option<Weak<AddressSpace>>,
    self_ref: Weak<LockedVMA>,
}

impl core::hash::Hash for VMA {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.region.hash(state);
        self.flags.hash(state);
        self.mapped.hash(state);
    }
}

impl VMA {
    pub fn region(&self) -> &VirtRegion {
        return &self.region;
    }

    /// # 拷贝当前VMA的内容
    ///
    /// ### 安全性
    ///
    /// 由于这样操作可能由于错误的拷贝，导致内存泄露、内存重复释放等问题，所以需要小心使用。
    pub unsafe fn clone(&self) -> Self {
        return Self {
            region: self.region,
            flags: self.flags,
            mapped: self.mapped,
            user_address_space: self.user_address_space.clone(),
            self_ref: self.self_ref.clone(),
        };
    }

    /// 把物理地址映射到虚拟地址
    ///
    /// @param phys 要映射的物理地址
    /// @param destination 要映射到的虚拟地址
    /// @param count 要映射的页帧数量
    /// @param flags 页面标志位
    /// @param mapper 页表映射器
    /// @param flusher 页表项刷新器
    ///
    /// @return 返回映射后的虚拟内存区域
    pub fn physmap(
        phys: PhysPageFrame,
        destination: VirtPageFrame,
        count: PageFrameCount,
        flags: PageFlags<MMArch>,
        mapper: &mut PageMapper,
        mut flusher: impl Flusher<MMArch>,
    ) -> Result<Arc<LockedVMA>, SystemError> {
        {
            let mut cur_phy = phys;
            let mut cur_dest = destination;

            for _ in 0..count.data() {
                // 将物理页帧映射到虚拟页帧
                let r = unsafe {
                    mapper.map_phys(cur_dest.virt_address(), cur_phy.phys_address(), flags)
                }
                .expect("Failed to map phys, may be OOM error");

                // todo: 增加OOM处理

                // todo: 将VMA加入到anon_vma中

                // 刷新TLB
                flusher.consume(r);

                cur_phy = cur_phy.next();
                cur_dest = cur_dest.next();
            }
        }

        let r: Arc<LockedVMA> = LockedVMA::new(VMA {
            region: VirtRegion::new(destination.virt_address(), count.data() * MMArch::PAGE_SIZE),
            flags,
            mapped: true,
            user_address_space: None,
            self_ref: Weak::default(),
        });
        return Ok(r);
    }

    /// 从页分配器中分配一些物理页，并把它们映射到指定的虚拟地址，然后创建VMA
    ///
    /// @param destination 要映射到的虚拟地址
    /// @param count 要映射的页帧数量
    /// @param flags 页面标志位
    /// @param mapper 页表映射器
    /// @param flusher 页表项刷新器
    ///
    /// @return 返回映射后的虚拟内存区域
    pub fn zeroed(
        destination: VirtPageFrame,
        page_count: PageFrameCount,
        flags: PageFlags<MMArch>,
        mapper: &mut PageMapper,
        mut flusher: impl Flusher<MMArch>,
    ) -> Result<Arc<LockedVMA>, SystemError> {
        let mut cur_dest = destination;
        for _ in 0..page_count.data() {
            let r = unsafe { mapper.map(cur_dest.virt_address(), flags) }
                .expect("Failed to map zero, may be OOM error");
            // todo: 将VMA加入到anon_vma中

            // todo: 增加OOM处理

            // 刷新TLB
            flusher.consume(r);
            cur_dest = cur_dest.next();
        }
        let r = LockedVMA::new(VMA {
            region: VirtRegion::new(
                destination.virt_address(),
                page_count.data() * MMArch::PAGE_SIZE,
            ),
            flags,
            mapped: true,
            user_address_space: None,
            self_ref: Weak::default(),
        });
        return Ok(r);
    }
}

impl Drop for VMA {
    fn drop(&mut self) {
        // 当VMA被释放时，需要确保它已经被从页表中解除映射
        assert!(!self.mapped, "VMA is still mapped");
    }
}

impl PartialEq for VMA {
    fn eq(&self, other: &Self) -> bool {
        return self.region == other.region;
    }
}

impl Eq for VMA {}

impl PartialOrd for VMA {
    fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
        return self.region.partial_cmp(&other.region);
    }
}

impl Ord for VMA {
    fn cmp(&self, other: &Self) -> cmp::Ordering {
        return self.region.cmp(&other.region);
    }
}
