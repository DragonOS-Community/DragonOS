// 进程的用户空间内存管理

use core::{
    cmp,
    hash::Hasher,
    intrinsics::unlikely,
    ops::Add,
    sync::atomic::{compiler_fence, Ordering},
};

use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
    vec::Vec,
};
use hashbrown::HashSet;
use system_error::SystemError;

use crate::{
    arch::{mm::PageMapper, CurrentIrqArch, MMArch},
    exception::InterruptArch,
    libs::{
        align::page_align_up,
        rwlock::{RwLock, RwLockWriteGuard},
        spinlock::{SpinLock, SpinLockGuard},
    },
    process::ProcessManager,
    syscall::user_access::{UserBufferReader, UserBufferWriter},
};

use super::{
    allocator::page_frame::{
        deallocate_page_frames, PageFrameCount, PhysPageFrame, VirtPageFrame, VirtPageFrameIter,
    },
    page::{Flusher, InactiveFlusher, PageFlags, PageFlushAll},
    syscall::{MapFlags, MremapFlags, ProtFlags},
    MemoryManagementArch, PageTableKind, VirtAddr, VirtRegion, VmFlags,
};

/// MMAP_MIN_ADDR的默认值
/// 以下内容来自linux-5.19:
///  This is the portion of low virtual memory which should be protected
//   from userspace allocation.  Keeping a user from writing to low pages
//   can help reduce the impact of kernel NULL pointer bugs.
//   For most ia64, ppc64 and x86 users with lots of address space
//   a value of 65536 is reasonable and should cause no problems.
//   On arm and other archs it should not be higher than 32768.
//   Programs which use vm86 functionality or have some need to map
//   this low address space will need CAP_SYS_RAWIO or disable this
//   protection by setting the value to 0.
pub const DEFAULT_MMAP_MIN_ADDR: usize = 65536;

#[derive(Debug)]
pub struct AddressSpace {
    inner: RwLock<InnerAddressSpace>,
}

impl AddressSpace {
    pub fn new(create_stack: bool) -> Result<Arc<Self>, SystemError> {
        let inner = InnerAddressSpace::new(create_stack)?;
        let result = Self {
            inner: RwLock::new(inner),
        };
        return Ok(Arc::new(result));
    }

    /// 从pcb中获取当前进程的地址空间结构体的Arc指针
    pub fn current() -> Result<Arc<AddressSpace>, SystemError> {
        let vm = ProcessManager::current_pcb()
            .basic()
            .user_vm()
            .expect("Current process has no address space");

        return Ok(vm);
    }

    /// 判断某个地址空间是否为当前进程的地址空间
    pub fn is_current(self: &Arc<Self>) -> bool {
        let current = Self::current();
        if let Ok(current) = current {
            return Arc::ptr_eq(&current, self);
        }
        return false;
    }
}

impl core::ops::Deref for AddressSpace {
    type Target = RwLock<InnerAddressSpace>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl core::ops::DerefMut for AddressSpace {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

/// @brief 用户地址空间结构体（每个进程都有一个）
#[derive(Debug)]
pub struct InnerAddressSpace {
    pub user_mapper: UserMapper,
    pub mappings: UserMappings,
    pub mmap_min: VirtAddr,
    /// 用户栈信息结构体
    pub user_stack: Option<UserStack>,

    pub elf_brk_start: VirtAddr,
    pub elf_brk: VirtAddr,

    /// 当前进程的堆空间的起始地址
    pub brk_start: VirtAddr,
    /// 当前进程的堆空间的结束地址(不包含)
    pub brk: VirtAddr,

    pub start_code: VirtAddr,
    pub end_code: VirtAddr,
    pub start_data: VirtAddr,
    pub end_data: VirtAddr,
}

impl InnerAddressSpace {
    pub fn new(create_stack: bool) -> Result<Self, SystemError> {
        let mut result = Self {
            user_mapper: MMArch::setup_new_usermapper()?,
            mappings: UserMappings::new(),
            mmap_min: VirtAddr(DEFAULT_MMAP_MIN_ADDR),
            elf_brk_start: VirtAddr::new(0),
            elf_brk: VirtAddr::new(0),
            brk_start: MMArch::USER_BRK_START,
            brk: MMArch::USER_BRK_START,
            user_stack: None,
            start_code: VirtAddr(0),
            end_code: VirtAddr(0),
            start_data: VirtAddr(0),
            end_data: VirtAddr(0),
        };
        if create_stack {
            // kdebug!("to create user stack.");
            result.new_user_stack(UserStack::DEFAULT_USER_STACK_SIZE)?;
        }

        return Ok(result);
    }

    /// 尝试克隆当前进程的地址空间，包括这些映射都会被克隆
    ///
    /// # Returns
    ///
    /// 返回克隆后的，新的地址空间的Arc指针
    #[inline(never)]
    pub fn try_clone(&mut self) -> Result<Arc<AddressSpace>, SystemError> {
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        let new_addr_space = AddressSpace::new(false)?;
        let mut new_guard = new_addr_space.write();

        // 拷贝用户栈的结构体信息，但是不拷贝用户栈的内容（因为后面VMA的拷贝会拷贝用户栈的内容）
        unsafe {
            new_guard.user_stack = Some(self.user_stack.as_ref().unwrap().clone_info_only());
        }
        let _current_stack_size = self.user_stack.as_ref().unwrap().stack_size();

        let current_mapper = &mut self.user_mapper.utable;

        // 拷贝空洞
        new_guard.mappings.vm_holes = self.mappings.vm_holes.clone();

        for vma in self.mappings.vmas.iter() {
            // TODO: 增加对VMA是否为文件映射的判断，如果是的话，就跳过

            let vma_guard: SpinLockGuard<'_, VMA> = vma.lock();
            let old_flags = vma_guard.flags();
            let tmp_flags: PageFlags<MMArch> = PageFlags::new().set_write(true);

            // 分配内存页并创建新的VMA
            let new_vma = VMA::zeroed(
                VirtPageFrame::new(vma_guard.region.start()),
                PageFrameCount::new(vma_guard.region.size() / MMArch::PAGE_SIZE),
                vma_guard.vm_flags().clone(),
                tmp_flags,
                &mut new_guard.user_mapper.utable,
                (),
            )?;
            new_guard.mappings.vmas.insert(new_vma.clone());
            // kdebug!("new vma: {:x?}", new_vma);
            let mut new_vma_guard = new_vma.lock();
            for page in new_vma_guard.pages().map(|p| p.virt_address()) {
                // kdebug!("page: {:x?}", page);
                let current_frame = unsafe {
                    MMArch::phys_2_virt(
                        current_mapper
                            .translate(page)
                            .expect("VMA page not mapped")
                            .0,
                    )
                }
                .expect("Phys2Virt: vaddr overflow.")
                .data() as *mut u8;

                let new_frame = unsafe {
                    MMArch::phys_2_virt(
                        new_guard
                            .user_mapper
                            .utable
                            .translate(page)
                            .expect("VMA page not mapped")
                            .0,
                    )
                }
                .expect("Phys2Virt: vaddr overflow.")
                .data() as *mut u8;

                unsafe {
                    // 拷贝数据
                    new_frame.copy_from_nonoverlapping(current_frame, MMArch::PAGE_SIZE);
                }
            }
            drop(vma_guard);

            new_vma_guard.remap(old_flags, &mut new_guard.user_mapper.utable, ())?;
            drop(new_vma_guard);
        }
        drop(new_guard);
        drop(irq_guard);
        return Ok(new_addr_space);
    }

    /// 判断当前的地址空间是否是当前进程的地址空间
    #[inline]
    pub fn is_current(&self) -> bool {
        return self.user_mapper.utable.is_current();
    }

    /// 进行匿名页映射
    ///
    /// ## 参数
    ///
    /// - `start_vaddr`：映射的起始地址
    /// - `len`：映射的长度
    /// - `prot_flags`：保护标志
    /// - `map_flags`：映射标志
    /// - `round_to_min`：是否将`start_vaddr`对齐到`mmap_min`，如果为`true`，则当`start_vaddr`不为0时，会对齐到`mmap_min`，否则仅向下对齐到页边界
    ///
    /// ## 返回
    ///
    /// 返回映射的起始虚拟页帧
    pub fn map_anonymous(
        &mut self,
        start_vaddr: VirtAddr,
        len: usize,
        prot_flags: ProtFlags,
        map_flags: MapFlags,
        round_to_min: bool,
    ) -> Result<VirtPageFrame, SystemError> {
        // 用于对齐hint的函数
        let round_hint_to_min = |hint: VirtAddr| {
            // 先把hint向下对齐到页边界
            let addr = hint.data() & (!MMArch::PAGE_OFFSET_MASK);
            // kdebug!("map_anonymous: hint = {:?}, addr = {addr:#x}", hint);
            // 如果hint不是0，且hint小于DEFAULT_MMAP_MIN_ADDR，则对齐到DEFAULT_MMAP_MIN_ADDR
            if (addr != 0) && round_to_min && (addr < DEFAULT_MMAP_MIN_ADDR) {
                Some(VirtAddr::new(page_align_up(DEFAULT_MMAP_MIN_ADDR)))
            } else if addr == 0 {
                None
            } else {
                Some(VirtAddr::new(addr))
            }
        };
        // kdebug!("map_anonymous: start_vaddr = {:?}", start_vaddr);
        // kdebug!("map_anonymous: len(no align) = {}", len);

        let len = page_align_up(len);

        let vm_flags = VmFlags::from(prot_flags)
            | VmFlags::from(map_flags)
            | VmFlags::VM_MAYREAD
            | VmFlags::VM_MAYWRITE
            | VmFlags::VM_MAYEXEC;

        // kdebug!("map_anonymous: len = {}", len);

        let start_page: VirtPageFrame = self.mmap(
            round_hint_to_min(start_vaddr),
            PageFrameCount::from_bytes(len).unwrap(),
            prot_flags,
            map_flags,
            move |page, count, flags, mapper, flusher| {
                Ok(VMA::zeroed(page, count, vm_flags, flags, mapper, flusher)?)
            },
        )?;

        return Ok(start_page);
    }

    /// 向进程的地址空间映射页面
    ///
    /// # 参数
    ///
    /// - `addr`：映射的起始地址，如果为`None`，则由内核自动分配
    /// - `page_count`：映射的页面数量
    /// - `prot_flags`：保护标志
    /// - `map_flags`：映射标志
    /// - `map_func`：映射函数，用于创建VMA
    ///
    /// # Returns
    ///
    /// 返回映射的起始虚拟页帧
    ///
    /// # Errors
    ///
    /// - `EINVAL`：参数错误
    pub fn mmap<
        F: FnOnce(
            VirtPageFrame,
            PageFrameCount,
            PageFlags<MMArch>,
            &mut PageMapper,
            &mut dyn Flusher<MMArch>,
        ) -> Result<Arc<LockedVMA>, SystemError>,
    >(
        &mut self,
        addr: Option<VirtAddr>,
        page_count: PageFrameCount,
        prot_flags: ProtFlags,
        map_flags: MapFlags,
        map_func: F,
    ) -> Result<VirtPageFrame, SystemError> {
        if page_count == PageFrameCount::new(0) {
            return Err(SystemError::EINVAL);
        }
        // kdebug!("mmap: addr: {addr:?}, page_count: {page_count:?}, prot_flags: {prot_flags:?}, map_flags: {map_flags:?}");

        // 找到未使用的区域
        let region = match addr {
            Some(vaddr) => {
                self.mappings
                    .find_free_at(self.mmap_min, vaddr, page_count.bytes(), map_flags)?
            }
            None => self
                .mappings
                .find_free(self.mmap_min, page_count.bytes())
                .ok_or(SystemError::ENOMEM)?,
        };

        let page = VirtPageFrame::new(region.start());

        // kdebug!("mmap: page: {:?}, region={region:?}", page.virt_address());

        compiler_fence(Ordering::SeqCst);
        let (mut active, mut inactive);
        let flusher = if self.is_current() {
            active = PageFlushAll::new();
            &mut active as &mut dyn Flusher<MMArch>
        } else {
            inactive = InactiveFlusher::new();
            &mut inactive as &mut dyn Flusher<MMArch>
        };
        compiler_fence(Ordering::SeqCst);
        // 映射页面，并将VMA插入到地址空间的VMA列表中
        self.mappings.insert_vma(map_func(
            page,
            page_count,
            PageFlags::from_prot_flags(prot_flags, true),
            &mut self.user_mapper.utable,
            flusher,
        )?);

        return Ok(page);
    }

    /// 重映射内存区域
    ///
    /// # 参数
    ///
    /// - `old_vaddr`：原映射的起始地址
    /// - `old_len`：原映射的长度
    /// - `new_len`：重新映射的长度
    /// - `mremap_flags`：重映射标志
    /// - `new_vaddr`：重新映射的起始地址
    /// - `vm_flags`：旧内存区域标志
    ///
    /// # Returns
    ///
    /// 返回重映射的起始虚拟页帧地址
    ///
    /// # Errors
    ///
    /// - `EINVAL`：参数错误
    pub fn mremap(
        &mut self,
        old_vaddr: VirtAddr,
        old_len: usize,
        new_len: usize,
        mremap_flags: MremapFlags,
        new_vaddr: VirtAddr,
        vm_flags: VmFlags,
    ) -> Result<VirtAddr, SystemError> {
        // 检查新内存地址是否对齐
        if !new_vaddr.check_aligned(MMArch::PAGE_SIZE) {
            return Err(SystemError::EINVAL);
        }

        // 检查新、旧内存区域是否冲突
        let old_region = VirtRegion::new(old_vaddr, old_len);
        let new_region = VirtRegion::new(new_vaddr, new_len);
        if old_region.collide(&new_region) {
            return Err(SystemError::EINVAL);
        }

        // 初始化映射标志
        let mut map_flags: MapFlags = vm_flags.into();
        // 初始化内存区域保护标志
        let prot_flags: ProtFlags = vm_flags.into();

        // 取消新内存区域的原映射
        if mremap_flags.contains(MremapFlags::MREMAP_FIXED) {
            map_flags |= MapFlags::MAP_FIXED;
            let start_page = VirtPageFrame::new(new_vaddr);
            let page_count = PageFrameCount::from_bytes(new_len).unwrap();
            self.munmap(start_page, page_count)?;
        }

        // 获取映射后的新内存页面
        let new_page = self.map_anonymous(new_vaddr, new_len, prot_flags, map_flags, true)?;
        let new_page_vaddr = new_page.virt_address();

        // 拷贝旧内存区域内容到新内存区域
        let old_buffer_reader =
            UserBufferReader::new(old_vaddr.data() as *const u8, old_len, true)?;
        let old_buf: &[u8] = old_buffer_reader.read_from_user(0)?;
        let mut new_buffer_writer =
            UserBufferWriter::new(new_page_vaddr.data() as *mut u8, new_len, true)?;
        let new_buf: &mut [u8] = new_buffer_writer.buffer(0)?;
        let len = old_buf.len().min(new_buf.len());
        for i in 0..len {
            new_buf[i] = old_buf[i];
        }

        return Ok(new_page_vaddr);
    }

    /// 取消进程的地址空间中的映射
    ///
    /// # 参数
    ///
    /// - `start_page`：起始页帧
    /// - `page_count`：取消映射的页帧数量
    ///
    /// # Errors
    ///
    /// - `EINVAL`：参数错误
    /// - `ENOMEM`：内存不足
    pub fn munmap(
        &mut self,
        start_page: VirtPageFrame,
        page_count: PageFrameCount,
    ) -> Result<(), SystemError> {
        let to_unmap = VirtRegion::new(start_page.virt_address(), page_count.bytes());
        let mut flusher: PageFlushAll<MMArch> = PageFlushAll::new();

        let regions: Vec<Arc<LockedVMA>> = self.mappings.conflicts(to_unmap).collect::<Vec<_>>();

        for r in regions {
            let r = r.lock().region;
            let r = self.mappings.remove_vma(&r).unwrap();
            let intersection = r.lock().region().intersect(&to_unmap).unwrap();
            let (before, r, after) = r.extract(intersection).unwrap();

            // TODO: 当引入后备页映射后，这里需要增加通知文件的逻辑

            if let Some(before) = before {
                // 如果前面有VMA，则需要将前面的VMA重新插入到地址空间的VMA列表中
                self.mappings.insert_vma(before);
            }

            if let Some(after) = after {
                // 如果后面有VMA，则需要将后面的VMA重新插入到地址空间的VMA列表中
                self.mappings.insert_vma(after);
            }

            r.unmap(&mut self.user_mapper.utable, &mut flusher);
        }

        // TODO: 当引入后备页映射后，这里需要增加通知文件的逻辑

        return Ok(());
    }

    pub fn mprotect(
        &mut self,
        start_page: VirtPageFrame,
        page_count: PageFrameCount,
        prot_flags: ProtFlags,
    ) -> Result<(), SystemError> {
        // kdebug!(
        //     "mprotect: start_page: {:?}, page_count: {:?}, prot_flags:{prot_flags:?}",
        //     start_page,
        //     page_count
        // );
        let (mut active, mut inactive);
        let mut flusher = if self.is_current() {
            active = PageFlushAll::new();
            &mut active as &mut dyn Flusher<MMArch>
        } else {
            inactive = InactiveFlusher::new();
            &mut inactive as &mut dyn Flusher<MMArch>
        };

        let mapper = &mut self.user_mapper.utable;
        let region = VirtRegion::new(start_page.virt_address(), page_count.bytes());
        // kdebug!("mprotect: region: {:?}", region);

        let regions = self.mappings.conflicts(region).collect::<Vec<_>>();
        // kdebug!("mprotect: regions: {:?}", regions);

        for r in regions {
            // kdebug!("mprotect: r: {:?}", r);
            let r = r.lock().region().clone();
            let r = self.mappings.remove_vma(&r).unwrap();

            let intersection = r.lock().region().intersect(&region).unwrap();
            let (before, r, after) = r.extract(intersection).expect("Failed to extract VMA");

            if let Some(before) = before {
                self.mappings.insert_vma(before);
            }
            if let Some(after) = after {
                self.mappings.insert_vma(after);
            }

            let mut r_guard = r.lock();
            // 如果VMA的保护标志不允许指定的修改，则返回错误
            if !r_guard.can_have_flags(prot_flags) {
                drop(r_guard);
                self.mappings.insert_vma(r.clone());
                return Err(SystemError::EACCES);
            }

            let new_flags: PageFlags<MMArch> = r_guard
                .flags()
                .set_execute(prot_flags.contains(ProtFlags::PROT_EXEC))
                .set_write(prot_flags.contains(ProtFlags::PROT_WRITE));

            r_guard.remap(new_flags, mapper, &mut flusher)?;
            drop(r_guard);
            self.mappings.insert_vma(r);
        }

        return Ok(());
    }

    /// 创建新的用户栈
    ///
    /// ## 参数
    ///
    /// - `size`：栈的大小
    pub fn new_user_stack(&mut self, size: usize) -> Result<(), SystemError> {
        assert!(self.user_stack.is_none(), "User stack already exists");
        let stack = UserStack::new(self, None, size)?;
        self.user_stack = Some(stack);
        return Ok(());
    }

    #[inline(always)]
    pub fn user_stack_mut(&mut self) -> Option<&mut UserStack> {
        return self.user_stack.as_mut();
    }

    /// 取消用户空间内的所有映射
    pub unsafe fn unmap_all(&mut self) {
        let mut flusher: PageFlushAll<MMArch> = PageFlushAll::new();
        for vma in self.mappings.iter_vmas() {
            vma.unmap(&mut self.user_mapper.utable, &mut flusher);
        }
    }

    /// 设置进程的堆的内存空间
    ///
    /// ## 参数
    ///
    /// - `new_brk`：新的堆的结束地址。需要满足页对齐要求，并且是用户空间地址，且大于等于当前的堆的起始地址
    ///
    /// ## 返回值
    ///
    /// 返回旧的堆的结束地址
    pub unsafe fn set_brk(&mut self, new_brk: VirtAddr) -> Result<VirtAddr, SystemError> {
        assert!(new_brk.check_aligned(MMArch::PAGE_SIZE));

        if !new_brk.check_user() || new_brk < self.brk_start {
            return Err(SystemError::EFAULT);
        }

        let old_brk = self.brk;

        if new_brk > self.brk {
            let len = new_brk - self.brk;
            let prot_flags = ProtFlags::PROT_READ | ProtFlags::PROT_WRITE | ProtFlags::PROT_EXEC;
            let map_flags = MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS | MapFlags::MAP_FIXED;
            self.map_anonymous(old_brk, len, prot_flags, map_flags, true)?;

            self.brk = new_brk;
            return Ok(old_brk);
        } else {
            let unmap_len = self.brk - new_brk;
            let unmap_start = new_brk;
            if unmap_len == 0 {
                return Ok(old_brk);
            }
            self.munmap(
                VirtPageFrame::new(unmap_start),
                PageFrameCount::from_bytes(unmap_len).unwrap(),
            )?;
            self.brk = new_brk;
            return Ok(old_brk);
        }
    }

    pub unsafe fn sbrk(&mut self, incr: isize) -> Result<VirtAddr, SystemError> {
        if incr == 0 {
            return Ok(self.brk);
        }

        let new_brk = if incr > 0 {
            self.brk + incr as usize
        } else {
            self.brk - (incr.abs() as usize)
        };

        let new_brk = VirtAddr::new(page_align_up(new_brk.data()));

        return self.set_brk(new_brk);
    }
}

impl Drop for InnerAddressSpace {
    fn drop(&mut self) {
        unsafe {
            self.unmap_all();
        }
    }
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
        unsafe {
            deallocate_page_frames(
                PhysPageFrame::new(self.utable.table().phys()),
                PageFrameCount::new(1),
            )
        };
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
            vm_holes: core::iter::once((VirtAddr::new(0), MMArch::USER_END_VADDR.data()))
                .collect::<BTreeMap<_, _>>(),
        };
    }

    /// 判断当前进程的VMA内，是否有包含指定的虚拟地址的VMA。
    ///
    /// 如果有，返回包含指定虚拟地址的VMA的Arc指针，否则返回None。
    #[allow(dead_code)]
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
        let _vaddr = min_vaddr;
        let mut iter = self
            .vm_holes
            .iter()
            .skip_while(|(hole_vaddr, hole_size)| hole_vaddr.add(**hole_size) <= min_vaddr);

        let (hole_vaddr, size) = iter.find(|(hole_vaddr, hole_size)| {
            // 计算当前空洞的可用大小
            let available_size: usize =
                if hole_vaddr <= &&min_vaddr && min_vaddr <= hole_vaddr.add(**hole_size) {
                    **hole_size - (min_vaddr - **hole_vaddr)
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
                // 如果指定了 MAP_FIXED_NOREPLACE 标志，由于所指定的地址无法成功建立映射，则放弃映射，不对地址做修正
                return Err(SystemError::EEXIST);
            }

            if flags.contains(MapFlags::MAP_FIXED) {
                // todo: 支持MAP_FIXED标志对已有的VMA进行覆盖
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
            self.vm_holes.range_mut(..=region.start()).next_back();

        if let Some((prev_hole_vaddr, prev_hole_size)) = prev_hole {
            let prev_hole_end = prev_hole_vaddr.add(*prev_hole_size);

            if prev_hole_end > region.start() {
                // 如果前一个空洞的结束地址大于当前空洞的起始地址，那么就需要调整前一个空洞的大小。
                *prev_hole_size = region.start().data() - prev_hole_vaddr.data();
            }

            if prev_hole_end > region.end() {
                // 如果前一个空洞的结束地址大于当前空洞的结束地址，那么就需要增加一个新的空洞。
                self.vm_holes
                    .insert(region.end(), prev_hole_end - region.end());
            }
        }
    }

    /// 在当前进程的地址空间中，释放一个指定大小的区域，使得该区域成为一个空洞。
    /// 该函数会修改vm_holes中的空洞信息。
    fn unreserve_hole(&mut self, region: &VirtRegion) {
        // 如果将要插入的空洞与后一个空洞相邻，那么就需要合并。
        let next_hole_size: Option<usize> = self.vm_holes.remove(&region.end());

        if let Some((_prev_hole_vaddr, prev_hole_size)) = self
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

#[allow(dead_code)]
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
        // todo: 如果当前vma与文件相关，完善文件相关的逻辑

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
            unsafe { deallocate_page_frames(PhysPageFrame::new(paddr), PageFrameCount::new(1)) };

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
    /// 虚拟内存区域标志
    vm_flags: VmFlags,
    /// VMA内的页帧的标志
    flags: PageFlags<MMArch>,
    /// VMA内的页帧是否已经映射到页表
    mapped: bool,
    /// VMA所属的用户地址空间
    user_address_space: Option<Weak<AddressSpace>>,
    self_ref: Weak<LockedVMA>,

    provider: Provider,
}

impl core::hash::Hash for VMA {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.region.hash(state);
        self.flags.hash(state);
        self.mapped.hash(state);
    }
}

/// 描述不同类型的内存提供者或资源
#[derive(Debug)]
pub enum Provider {
    Allocated, // TODO:其他
}

#[allow(dead_code)]
impl VMA {
    pub fn new(
        region: VirtRegion,
        vm_flags: VmFlags,
        flags: PageFlags<MMArch>,
        mapped: bool,
    ) -> Self {
        VMA {
            region,
            vm_flags,
            flags,
            mapped,
            user_address_space: None,
            self_ref: Weak::default(),
            provider: Provider::Allocated,
        }
    }

    pub fn region(&self) -> &VirtRegion {
        return &self.region;
    }

    pub fn vm_flags(&self) -> &VmFlags {
        return &self.vm_flags;
    }

    pub fn set_vm_flags(&mut self, vm_flags: VmFlags) {
        self.vm_flags = vm_flags;
    }

    pub fn set_region_size(&mut self, new_region_size: usize) {
        self.region.set_size(new_region_size);
    }

    /// # 拷贝当前VMA的内容
    ///
    /// ### 安全性
    ///
    /// 由于这样操作可能由于错误的拷贝，导致内存泄露、内存重复释放等问题，所以需要小心使用。
    pub unsafe fn clone(&self) -> Self {
        return Self {
            region: self.region,
            vm_flags: self.vm_flags,
            flags: self.flags,
            mapped: self.mapped,
            user_address_space: self.user_address_space.clone(),
            self_ref: self.self_ref.clone(),
            provider: Provider::Allocated,
        };
    }

    #[inline(always)]
    pub fn flags(&self) -> PageFlags<MMArch> {
        return self.flags;
    }

    pub fn pages(&self) -> VirtPageFrameIter {
        return VirtPageFrameIter::new(
            VirtPageFrame::new(self.region.start()),
            VirtPageFrame::new(self.region.end()),
        );
    }

    pub fn remap(
        &mut self,
        flags: PageFlags<MMArch>,
        mapper: &mut PageMapper,
        mut flusher: impl Flusher<MMArch>,
    ) -> Result<(), SystemError> {
        assert!(self.mapped);
        for page in self.region.pages() {
            // kdebug!("remap page {:?}", page.virt_address());
            // 暂时要求所有的页帧都已经映射到页表
            // TODO: 引入Lazy Mapping, 通过缺页中断来映射页帧，这里就不必要求所有的页帧都已经映射到页表了
            let r = unsafe {
                mapper
                    .remap(page.virt_address(), flags)
                    .expect("Failed to remap, beacuse of some page is not mapped")
            };
            // kdebug!("consume page {:?}", page.virt_address());
            flusher.consume(r);
            // kdebug!("remap page {:?} done", page.virt_address());
        }
        self.flags = flags;
        return Ok(());
    }

    /// 检查当前VMA是否可以拥有指定的标志位
    ///
    /// ## 参数
    ///
    /// - `prot_flags` 要检查的标志位
    pub fn can_have_flags(&self, prot_flags: ProtFlags) -> bool {
        let is_downgrade = (self.flags.has_write() || !prot_flags.contains(ProtFlags::PROT_WRITE))
            && (self.flags.has_execute() || !prot_flags.contains(ProtFlags::PROT_EXEC));

        match self.provider {
            Provider::Allocated { .. } => true,

            #[allow(unreachable_patterns)]
            _ => is_downgrade,
        }
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
        vm_flags: VmFlags,
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
            vm_flags,
            flags,
            mapped: true,
            user_address_space: None,
            self_ref: Weak::default(),
            provider: Provider::Allocated,
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
        vm_flags: VmFlags,
        flags: PageFlags<MMArch>,
        mapper: &mut PageMapper,
        mut flusher: impl Flusher<MMArch>,
    ) -> Result<Arc<LockedVMA>, SystemError> {
        let mut cur_dest: VirtPageFrame = destination;
        // kdebug!(
        //     "VMA::zeroed: page_count = {:?}, destination={destination:?}",
        //     page_count
        // );
        for _ in 0..page_count.data() {
            // kdebug!(
            //     "VMA::zeroed: cur_dest={cur_dest:?}, vaddr = {:?}",
            //     cur_dest.virt_address()
            // );
            let r = unsafe { mapper.map(cur_dest.virt_address(), flags) }
                .expect("Failed to map zero, may be OOM error");
            // todo: 将VMA加入到anon_vma中
            // todo: 增加OOM处理

            // 稍后再刷新TLB，这里取消刷新
            flusher.consume(r);
            cur_dest = cur_dest.next();
        }
        let r = LockedVMA::new(VMA {
            region: VirtRegion::new(
                destination.virt_address(),
                page_count.data() * MMArch::PAGE_SIZE,
            ),
            vm_flags,
            flags,
            mapped: true,
            user_address_space: None,
            self_ref: Weak::default(),
            provider: Provider::Allocated,
        });
        drop(flusher);
        // kdebug!("VMA::zeroed: flusher dropped");

        // 清空这些内存
        let virt_iter: VirtPageFrameIter =
            VirtPageFrameIter::new(destination, destination.add(page_count));
        for frame in virt_iter {
            let paddr = mapper.translate(frame.virt_address()).unwrap().0;

            unsafe {
                let vaddr = MMArch::phys_2_virt(paddr).unwrap();
                MMArch::write_bytes(vaddr, 0, MMArch::PAGE_SIZE);
            }
        }
        // kdebug!("VMA::zeroed: done");
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

#[derive(Debug)]
pub struct UserStack {
    // 栈底地址
    stack_bottom: VirtAddr,
    // 当前已映射的大小
    mapped_size: usize,
    /// 栈顶地址（这个值需要仔细确定！因为它可能不会实时与用户栈的真实栈顶保持一致！要小心！）
    current_sp: VirtAddr,
}

impl UserStack {
    /// 默认的用户栈底地址
    pub const DEFAULT_USER_STACK_BOTTOM: VirtAddr = MMArch::USER_STACK_START;
    /// 默认的用户栈大小为8MB
    pub const DEFAULT_USER_STACK_SIZE: usize = 8 * 1024 * 1024;
    /// 用户栈的保护页数量
    pub const GUARD_PAGES_NUM: usize = 4;

    /// 创建一个用户栈
    pub fn new(
        vm: &mut InnerAddressSpace,
        stack_bottom: Option<VirtAddr>,
        stack_size: usize,
    ) -> Result<Self, SystemError> {
        let stack_bottom = stack_bottom.unwrap_or(Self::DEFAULT_USER_STACK_BOTTOM);
        assert!(stack_bottom.check_aligned(MMArch::PAGE_SIZE));

        // 分配用户栈的保护页
        let guard_size = Self::GUARD_PAGES_NUM * MMArch::PAGE_SIZE;
        let actual_stack_bottom = stack_bottom - guard_size;

        let mut prot_flags = ProtFlags::PROT_READ | ProtFlags::PROT_WRITE;
        let map_flags =
            MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS | MapFlags::MAP_FIXED_NOREPLACE;
        // kdebug!(
        //     "map anonymous stack: {:?} {}",
        //     actual_stack_bottom,
        //     guard_size
        // );
        vm.map_anonymous(
            actual_stack_bottom,
            guard_size,
            prot_flags,
            map_flags,
            false,
        )?;
        // test_buddy();
        // 设置保护页只读
        prot_flags.remove(ProtFlags::PROT_WRITE);
        // kdebug!(
        //     "to mprotect stack guard pages: {:?} {}",
        //     actual_stack_bottom,
        //     guard_size
        // );
        vm.mprotect(
            VirtPageFrame::new(actual_stack_bottom),
            PageFrameCount::new(Self::GUARD_PAGES_NUM),
            prot_flags,
        )?;

        // kdebug!(
        //     "mprotect stack guard pages done: {:?} {}",
        //     actual_stack_bottom,
        //     guard_size
        // );

        let mut user_stack = UserStack {
            stack_bottom: actual_stack_bottom,
            mapped_size: guard_size,
            current_sp: actual_stack_bottom - guard_size,
        };

        // kdebug!("extend user stack: {:?} {}", stack_bottom, stack_size);
        // 分配用户栈
        user_stack.initial_extend(vm, stack_size)?;
        // kdebug!("user stack created: {:?} {}", stack_bottom, stack_size);
        return Ok(user_stack);
    }

    fn initial_extend(
        &mut self,
        vm: &mut InnerAddressSpace,
        mut bytes: usize,
    ) -> Result<(), SystemError> {
        let prot_flags = ProtFlags::PROT_READ | ProtFlags::PROT_WRITE | ProtFlags::PROT_EXEC;
        let map_flags = MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS;

        bytes = page_align_up(bytes);
        self.mapped_size += bytes;

        vm.map_anonymous(
            self.stack_bottom - self.mapped_size,
            bytes,
            prot_flags,
            map_flags,
            false,
        )?;

        return Ok(());
    }

    /// 扩展用户栈
    ///
    /// ## 参数
    ///
    /// - `vm` 用户地址空间结构体
    /// - `bytes` 要扩展的字节数
    ///
    /// ## 返回值
    ///
    /// - **Ok(())** 扩展成功
    /// - **Err(SystemError)** 扩展失败
    #[allow(dead_code)]
    pub fn extend(
        &mut self,
        vm: &mut RwLockWriteGuard<InnerAddressSpace>,
        mut bytes: usize,
    ) -> Result<(), SystemError> {
        let prot_flags = ProtFlags::PROT_READ | ProtFlags::PROT_WRITE | ProtFlags::PROT_EXEC;
        let map_flags = MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS;

        bytes = page_align_up(bytes);
        self.mapped_size += bytes;

        vm.map_anonymous(
            self.stack_bottom - self.mapped_size,
            bytes,
            prot_flags,
            map_flags,
            false,
        )?;

        return Ok(());
    }

    /// 获取栈顶地址
    ///
    /// 请注意，如果用户栈的栈顶地址发生变化，这个值可能不会实时更新！
    pub fn sp(&self) -> VirtAddr {
        return self.current_sp;
    }

    pub unsafe fn set_sp(&mut self, sp: VirtAddr) {
        self.current_sp = sp;
    }

    /// 仅仅克隆用户栈的信息，不会克隆用户栈的内容/映射
    pub unsafe fn clone_info_only(&self) -> Self {
        return Self {
            stack_bottom: self.stack_bottom,
            mapped_size: self.mapped_size,
            current_sp: self.current_sp,
        };
    }

    /// 获取当前用户栈的大小（不包括保护页）
    pub fn stack_size(&self) -> usize {
        return self.mapped_size - Self::GUARD_PAGES_NUM * MMArch::PAGE_SIZE;
    }
}
