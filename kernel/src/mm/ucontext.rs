// 进程的用户空间内存管理

use core::{
    cmp,
    hash::Hasher,
    intrinsics::unlikely,
    ops::Add,
    sync::atomic::{compiler_fence, AtomicU64, Ordering},
};

use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
    vec::Vec,
};
use defer::defer;
use hashbrown::HashMap;
use hashbrown::HashSet;
use ida::IdAllocator;
use log::warn;
use system_error::SystemError;

use crate::{
    arch::{mm::PageMapper, CurrentIrqArch, MMArch},
    exception::InterruptArch,
    filesystem::vfs::{
        file::{File, FileMode},
        FileType, InodeId,
    },
    ipc::shm::{ShmFlags, ShmId},
    libs::{
        align::page_align_up,
        mutex::{Mutex, MutexGuard},
        rwsem::RwSem,
        spinlock::SpinLock,
    },
    mm::{page::page_manager_lock, PhysAddr},
    process::{resource::RLimitID, ProcessManager},
};

use super::{
    allocator::page_frame::{
        deallocate_page_frames, PageFrameCount, PhysPageFrame, VirtPageFrame, VirtPageFrameIter,
    },
    page::{EntryFlags, Flusher, InactiveFlusher, Page, PageFlags, PageFlushAll, PageType},
    syscall::{MadvFlags, MapFlags, MremapFlags, ProtFlags},
    MemoryManagementArch, PageTableKind, VirtAddr, VirtRegion, VmFlags,
};
use crate::arch::mm::LockedFrameAllocator;

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

/// LockedVMA的id分配器
static LOCKEDVMA_ID_ALLOCATOR: SpinLock<IdAllocator> =
    SpinLock::new(IdAllocator::new(0, usize::MAX).unwrap());

/// AddressSpace的全局唯一ID分配器
/// 用于为每个地址空间分配一个全局唯一且递增的ID
static ADDRESS_SPACE_ID_ALLOCATOR: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub struct AddressSpace {
    /// 全局唯一的地址空间ID，用于标识不同的地址空间
    /// 该ID在地址空间的整个生命周期内保持不变，且永不重复
    id: u64,
    /// 页表物理地址（创建后不变，可无锁访问）
    /// 用于在调度器上下文中快速切换页表，无需获取RwSem锁
    table_paddr: PhysAddr,
    /// 使用RwSem而非RwLock，因为地址空间操作可能需要进行I/O（如页缺失时的文件读取）
    inner: RwSem<InnerAddressSpace>,
}

impl AddressSpace {
    pub fn new(create_stack: bool) -> Result<Arc<Self>, SystemError> {
        let inner = InnerAddressSpace::new(create_stack)?;
        let table_paddr = inner.user_mapper.utable.table().phys();
        let id = ADDRESS_SPACE_ID_ALLOCATOR.fetch_add(1, Ordering::Relaxed);
        let result = Self {
            id,
            table_paddr,
            inner: RwSem::new(inner),
        };
        return Ok(Arc::new(result));
    }

    /// 获取地址空间的全局唯一ID
    #[inline(always)]
    pub fn id(&self) -> u64 {
        self.id
    }

    /// 获取页表物理地址（无锁访问）
    /// 用于在调度器上下文中快速切换页表
    #[inline(always)]
    pub fn table_paddr(&self) -> PhysAddr {
        self.table_paddr
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

    /// 将此地址空间的页表设置为当前页表（无锁）
    ///
    /// 此方法用于调度器上下文中的快速页表切换，无需获取RwSem锁。
    /// 安全性由调用者保证：只在进程切换时使用。
    #[inline(always)]
    pub unsafe fn make_current(&self) {
        MMArch::set_table(PageTableKind::User, self.table_paddr);
    }
}

impl core::ops::Deref for AddressSpace {
    type Target = RwSem<InnerAddressSpace>;

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

    /// 有 VM_LOCKED 标志的 VMA 页面数
    locked_vm: usize,

    /// mlockall 默认标志 (影响未来映射)
    def_flags: VmFlags,
}

impl InnerAddressSpace {
    /// 当前地址空间已占用的虚拟内存字节数（简单求和所有 VMA 尺寸）
    pub fn vma_usage_bytes(&self) -> usize {
        self.mappings
            .iter_vmas()
            .map(|v| {
                let g = v.lock();
                g.region().size()
            })
            .sum()
    }

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
            locked_vm: 0,
            def_flags: VmFlags::empty(),
        };
        if create_stack {
            // debug!("to create user stack.");
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

        // 仅拷贝用户栈的结构体信息（元数据），实际的用户栈页面内容会在下面的 VMA 循环中处理
        unsafe {
            new_guard.user_stack = Some(self.user_stack.as_ref().unwrap().clone_info_only());
        }

        // 拷贝空洞
        new_guard.mappings.vm_holes = self.mappings.vm_holes.clone();

        // 拷贝其他地址空间属性
        new_guard.brk = self.brk;
        new_guard.brk_start = self.brk_start;
        new_guard.mmap_min = self.mmap_min;
        new_guard.elf_brk = self.elf_brk;
        new_guard.elf_brk_start = self.elf_brk_start;
        new_guard.start_code = self.start_code;
        new_guard.end_code = self.end_code;
        new_guard.start_data = self.start_data;
        new_guard.end_data = self.end_data;
        // 注意：locked_vm 在子进程中应该为 0，因为 mlock 不会被 fork 继承
        // 参考 Linux: 子进程的 mm->locked_vm 从 0 开始
        new_guard.locked_vm = 0;
        // 注意：def_flags 也不应该被继承
        // 参考 Linux: 子进程的 mm->def_flags 从 0 开始
        new_guard.def_flags = VmFlags::empty();

        // 遍历父进程的每个VMA，根据VMA属性进行适当的复制
        // 参考 Linux: https://code.dragonos.org.cn/xref/linux-6.6.21/mm/memory.c#copy_page_range
        for vma in self.mappings.vmas.iter() {
            let vma_guard = vma.lock();

            // VM_DONTCOPY: 跳过不复制的VMA (例如 MADV_DONTFORK 标记的)
            if vma_guard.vm_flags().contains(VmFlags::VM_DONTCOPY) {
                drop(vma_guard);
                continue;
            }

            let vm_flags = vma_guard.vm_flags();
            let is_shared = vm_flags.contains(VmFlags::VM_SHARED);
            let region = *vma_guard.region();
            let page_flags = vma_guard.flags();

            // 创建新的VMA
            let new_vma = LockedVMA::new(vma_guard.clone_info_only());
            new_guard.mappings.vmas.insert(new_vma.clone());

            // 根据VMA类型进行不同的页面复制策略
            let start_page = region.start();
            let end_page = region.end();
            let mut current_page = start_page;

            let old_mapper = &mut self.user_mapper.utable;
            let new_mapper = &mut new_guard.user_mapper.utable;
            let mut page_manager_guard = page_manager_lock();

            while current_page < end_page {
                if let Some((phys_addr, old_flags)) = old_mapper.translate(current_page) {
                    unsafe {
                        if is_shared {
                            // 共享映射：直接映射到相同的物理页，不使用COW
                            // 保持原有的flags
                            if new_mapper
                                .map_phys(current_page, phys_addr, page_flags)
                                .is_none()
                            {
                                warn!("Failed to map shared page at {:?} to phys {:?} in child process (current_pid: {:?})",
                                      current_page, phys_addr, ProcessManager::current_pcb().raw_pid());
                            }
                        } else {
                            // 私有映射：使用COW机制
                            // 将父进程和子进程的页表项都设置为只读
                            let cow_flags = page_flags.set_write(false);

                            // 更新父进程的页表项为只读
                            if old_flags.has_write() {
                                if let Some(flush) = old_mapper.remap(current_page, cow_flags) {
                                    flush.flush();
                                }
                            }

                            // 子进程也映射为只读
                            if new_mapper
                                .map_phys(current_page, phys_addr, cow_flags)
                                .is_none()
                            {
                                warn!("Failed to map COW page at {:?} to phys {:?} in child process (current_pid: {:?})",
                                      current_page, phys_addr, ProcessManager::current_pcb().raw_pid());
                            }
                        }
                        // 为新进程的VMA添加反向映射
                        if let Some(page) = page_manager_guard.get(&phys_addr) {
                            page.write_irqsave().insert_vma(new_vma.clone());
                        }
                    }
                }
                current_page = VirtAddr::new(current_page.data() + MMArch::PAGE_SIZE);
            }
            drop(page_manager_guard);

            drop(vma_guard);
        }

        drop(new_guard);
        drop(irq_guard);
        return Ok(new_addr_space);
    }

    /// Check if the stack can be extended
    pub fn can_extend_stack(&self, bytes: usize) -> bool {
        let bytes = page_align_up(bytes);
        let stack = self.user_stack.as_ref().unwrap();
        let new_size = stack.mapped_size + bytes;
        if new_size > stack.max_limit {
            // Don't exceed the maximum stack size
            return false;
        }
        return true;
    }

    /// 拓展用户栈
    /// ## 参数
    ///
    /// - `bytes`: 拓展大小
    pub fn extend_stack(&mut self, mut bytes: usize) -> Result<(), SystemError> {
        // log::debug!("extend user stack");

        // Layout
        // -------------- high->sp
        // | stack pages|
        // |------------|
        // | stack pages|
        // |------------|
        // | not mapped |
        // -------------- low

        let prot_flags = ProtFlags::PROT_READ | ProtFlags::PROT_WRITE | ProtFlags::PROT_EXEC;
        let map_flags = MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS | MapFlags::MAP_GROWSDOWN;
        let stack = self.user_stack.as_mut().unwrap();

        bytes = page_align_up(bytes);
        stack.mapped_size += bytes;
        // map new stack pages
        let extend_stack_start = stack.stack_bottom - stack.mapped_size;

        self.map_anonymous(
            extend_stack_start,
            bytes,
            prot_flags,
            map_flags,
            false,
            false,
        )?;
        return Ok(());
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
    /// - `allocate_at_once`：是否立即分配物理空间
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
        allocate_at_once: bool,
    ) -> Result<VirtPageFrame, SystemError> {
        let allocate_at_once = if MMArch::PAGE_FAULT_ENABLED {
            allocate_at_once
        } else {
            true
        };
        // 用于对齐hint的函数
        let round_hint_to_min = |hint: VirtAddr| {
            // 先把hint向下对齐到页边界
            let addr = hint.data() & (!MMArch::PAGE_OFFSET_MASK);
            // debug!("map_anonymous: hint = {:?}, addr = {addr:#x}", hint);
            // 如果hint不是0，且hint小于DEFAULT_MMAP_MIN_ADDR，则对齐到DEFAULT_MMAP_MIN_ADDR
            if (addr != 0) && round_to_min && (addr < DEFAULT_MMAP_MIN_ADDR) {
                Some(VirtAddr::new(page_align_up(DEFAULT_MMAP_MIN_ADDR)))
            } else if addr == 0 {
                None
            } else {
                Some(VirtAddr::new(addr))
            }
        };
        // debug!("map_anonymous: start_vaddr = {:?}", start_vaddr);
        // debug!("map_anonymous: len(no align) = {}", len);

        let len = page_align_up(len);

        // debug!("map_anonymous: len = {}", len);

        let start_page: VirtPageFrame = self.mmap(
            round_hint_to_min(start_vaddr),
            PageFrameCount::from_bytes(len).unwrap(),
            prot_flags,
            map_flags,
            move |page, count, vm_flags, flags, mapper, flusher| {
                if allocate_at_once {
                    let vma =
                        VMA::zeroed(page, count, vm_flags, flags, mapper, flusher, None, None)?;
                    // 如果是共享匿名映射，则分配稳定身份
                    if vm_flags.contains(VmFlags::VM_SHARED) {
                        let mut g = vma.lock();
                        g.shared_anon = Some(AnonSharedMapping::new(count.data()));
                        // Set backing_pgoff to 0 as the base offset for shared-anon mappings.
                        g.backing_pgoff = Some(0);
                    }
                    Ok(vma)
                } else {
                    let vma = LockedVMA::new(VMA::new(
                        VirtRegion::new(page.virt_address(), count.data() * MMArch::PAGE_SIZE),
                        vm_flags,
                        flags,
                        None,
                        None,
                        false,
                    ));
                    if vm_flags.contains(VmFlags::VM_SHARED) {
                        let mut g = vma.lock();
                        g.shared_anon = Some(AnonSharedMapping::new(count.data()));
                        g.backing_pgoff = Some(0);
                    }
                    Ok(vma)
                }
            },
        )?;

        return Ok(start_page);
    }

    /// 进行文件页映射
    ///
    /// ## 参数
    ///
    /// - `file`：要映射的文件（直接传入 File，而非通过 fd_table 查找）
    /// - `start_vaddr`：映射的起始地址
    /// - `len`：映射的长度
    /// - `prot_flags`：保护标志
    /// - `map_flags`：映射标志
    /// - `offset`：映射偏移量
    /// - `round_to_min`：是否将`start_vaddr`对齐到`mmap_min`，如果为`true`，则当`start_vaddr`不为0时，会对齐到`mmap_min`，否则仅向下对齐到页边界
    /// - `allocate_at_once`：是否立即分配物理空间（文件映射通常应为按需缺页；此参数仅在禁用缺页机制时被强制为 true）
    ///
    /// ## 返回
    ///
    /// 返回映射的起始虚拟页帧
    #[allow(clippy::too_many_arguments)]
    pub fn file_mapping_with_file(
        &mut self,
        file: Arc<File>,
        start_vaddr: VirtAddr,
        len: usize,
        prot_flags: ProtFlags,
        map_flags: MapFlags,
        offset: usize,
        round_to_min: bool,
        allocate_at_once: bool,
    ) -> Result<VirtPageFrame, SystemError> {
        let allocate_at_once = if MMArch::PAGE_FAULT_ENABLED {
            allocate_at_once
        } else {
            true
        };
        // 用于对齐hint的函数
        let round_hint_to_min = |hint: VirtAddr| {
            // 先把hint向下对齐到页边界
            let addr = hint.data() & (!MMArch::PAGE_OFFSET_MASK);
            // 如果hint不是0，且hint小于DEFAULT_MMAP_MIN_ADDR，则对齐到DEFAULT_MMAP_MIN_ADDR
            if (addr != 0) && round_to_min && (addr < DEFAULT_MMAP_MIN_ADDR) {
                Some(VirtAddr::new(page_align_up(DEFAULT_MMAP_MIN_ADDR)))
            } else if addr == 0 {
                None
            } else {
                Some(VirtAddr::new(addr))
            }
        };

        let len = page_align_up(len);

        // 权限检查遵循 Linux 语义：
        // - O_PATH 直接返回 EBADF
        // - 除 PROT_NONE 外，映射需要读权限；PROT_WRITE 另外需要写权限（MAP_PRIVATE 也需要读以便 COW）
        // - PROT_EXEC 视为读检查
        let file_mode = file.mode();
        if file_mode.contains(FileMode::FMODE_PATH) {
            return Err(SystemError::EBADF);
        }

        let wants_access = prot_flags != ProtFlags::PROT_NONE;
        if wants_access && !file_mode.contains(FileMode::FMODE_READ) {
            return Err(SystemError::EACCES);
        }
        if prot_flags.contains(ProtFlags::PROT_EXEC) && !file_mode.contains(FileMode::FMODE_READ) {
            return Err(SystemError::EACCES);
        }
        if prot_flags.contains(ProtFlags::PROT_WRITE) {
            if map_flags.contains(MapFlags::MAP_SHARED) {
                if !file_mode.contains(FileMode::FMODE_WRITE) {
                    return Err(SystemError::EACCES);
                }
            } else if !file_mode.contains(FileMode::FMODE_READ) {
                return Err(SystemError::EACCES);
            }
        }

        let meta = file.metadata()?;
        if matches!(meta.file_type, FileType::Pipe | FileType::Dir) {
            return Err(SystemError::ENODEV);
        }

        // offset需要4K对齐
        if (offset & (MMArch::PAGE_SIZE - 1)) != 0 {
            return Err(SystemError::EINVAL);
        }
        let pgoff = offset >> MMArch::PAGE_SHIFT;

        let page_count = PageFrameCount::from_bytes(len).unwrap();
        let start_page: VirtPageFrame = self.mmap(
            round_hint_to_min(start_vaddr),
            page_count,
            prot_flags,
            map_flags,
            |page, count, vm_flags, flags, mapper, flusher| {
                if allocate_at_once {
                    VMA::zeroed(
                        page,
                        count,
                        vm_flags,
                        flags,
                        mapper,
                        flusher,
                        Some(file.clone()),
                        Some(pgoff),
                    )
                } else {
                    Ok(LockedVMA::new(VMA::new(
                        VirtRegion::new(page.virt_address(), count.data() * MMArch::PAGE_SIZE),
                        vm_flags,
                        flags,
                        Some(file.clone()),
                        Some(pgoff),
                        false,
                    )))
                }
            },
        )?;

        // todo!(impl mmap for other file)
        // https://github.com/DragonOS-Community/DragonOS/pull/912#discussion_r1765334272
        // 传入实际映射后的起始虚拟地址，而非用户传入的 hint
        match file
            .inode()
            .mmap(start_page.virt_address().data(), len, offset)
        {
            Ok(_) => Ok(start_page),
            Err(SystemError::ENOSYS) => Ok(start_page), // 文件系统未实现 mmap，视为成功
            Err(SystemError::ENODEV) => {
                let _ = self.munmap(start_page, page_count);
                Err(SystemError::ENODEV)
            }
            Err(e) => {
                let _ = self.munmap(start_page, page_count);
                Err(e)
            }
        }
    }

    /// 进行文件页映射
    ///
    /// ## 参数
    ///
    /// - `start_vaddr`：映射的起始地址
    /// - `len`：映射的长度
    /// - `prot_flags`：保护标志
    /// - `map_flags`：映射标志
    /// - `fd`：文件描述符
    /// - `offset`：映射偏移量
    /// - `round_to_min`：是否将`start_vaddr`对齐到`mmap_min`，如果为`true`，则当`start_vaddr`不为0时，会对齐到`mmap_min`，否则仅向下对齐到页边界
    /// - `allocate_at_once`：是否立即分配物理空间
    ///
    /// ## 返回
    ///
    /// 返回映射的起始虚拟页帧
    #[allow(clippy::too_many_arguments)]
    pub fn file_mapping(
        &mut self,
        start_vaddr: VirtAddr,
        len: usize,
        prot_flags: ProtFlags,
        map_flags: MapFlags,
        fd: i32,
        offset: usize,
        round_to_min: bool,
        allocate_at_once: bool,
    ) -> Result<VirtPageFrame, SystemError> {
        let binding = ProcessManager::current_pcb().fd_table();
        let fd_table_guard = binding.read();

        let file = fd_table_guard.get_file_by_fd(fd);
        if file.is_none() {
            return Err(SystemError::EBADF);
        }
        // drop guard 以避免无法调度的问题
        drop(fd_table_guard);

        let file = file.unwrap();
        self.file_mapping_with_file(
            file,
            start_vaddr,
            len,
            prot_flags,
            map_flags,
            offset,
            round_to_min,
            allocate_at_once,
        )
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
            VmFlags,
            EntryFlags<MMArch>,
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
        // debug!("mmap: addr: {addr:?}, page_count: {page_count:?}, prot_flags: {prot_flags:?}, map_flags: {map_flags:?}");

        // 找到未使用的区域
        let region = match addr {
            Some(vaddr) => {
                self.find_free_at(self.mmap_min, vaddr, page_count.bytes(), map_flags)?
            }
            None => self
                .mappings
                .find_free(self.mmap_min, page_count.bytes())
                .ok_or(SystemError::ENOMEM)?,
        };

        let page = VirtPageFrame::new(region.start());

        let vm_flags = VmFlags::from(prot_flags)
            | VmFlags::from(map_flags)
            | VmFlags::VM_MAYREAD
            | VmFlags::VM_MAYWRITE
            | VmFlags::VM_MAYEXEC
            | self.def_flags; // 应用 mlockall(MCL_FUTURE) 设置的默认标志

        // RLIMIT_MEMLOCK 检查（针对 VM_LOCKED/VM_LOCKONFAULT）
        // 无论 VM_LOCKED 来自 MAP_LOCKED 标志还是 MCL_FUTURE 的 def_flags，都需要检查限制
        // 参考 Linux: mm/mmap.c:mmap_region() 中的 security check
        let has_locked_flag =
            vm_flags.contains(VmFlags::VM_LOCKED) || vm_flags.contains(VmFlags::VM_LOCKONFAULT);
        if has_locked_flag {
            use crate::mm::mlock::can_do_mlock;

            // 权限检查
            if !can_do_mlock() {
                return Err(SystemError::EPERM);
            }

            // 检查 RLIMIT_MEMLOCK 是否足够
            let lock_limit = ProcessManager::current_pcb()
                .get_rlimit(RLimitID::Memlock)
                .rlim_cur as usize;

            // 将限制转换为页面数
            let lock_limit_pages = if lock_limit == usize::MAX {
                usize::MAX
            } else {
                lock_limit >> MMArch::PAGE_SHIFT
            };

            let requested_pages = page_count.data();

            // 获取当前地址空间的锁定计数
            let current_locked = self.locked_vm();

            // 检查是否超过限制
            if current_locked + requested_pages > lock_limit_pages {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
        }

        // debug!("mmap: page: {:?}, region={region:?}", page.virt_address());

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
        let vma = map_func(
            page,
            page_count,
            vm_flags,
            EntryFlags::from_prot_flags(prot_flags, true),
            &mut self.user_mapper.utable,
            flusher,
        )?;
        self.mappings.insert_vma(vma.clone());

        // 更新 locked_vm 计数（如果设置了 VM_LOCKED 或 VM_LOCKONFAULT）
        // 参考 Linux: mm/mmap.c:mmap_region() 中的 accounting
        if vm_flags.contains(VmFlags::VM_LOCKED) || vm_flags.contains(VmFlags::VM_LOCKONFAULT) {
            let page_count = page_count.data();
            self.locked_vm += page_count;
        }

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
    ) -> Result<VirtAddr, SystemError> {
        // 仅在 MREMAP_FIXED 下需要检查 new_vaddr（否则 new_vaddr 参数应被忽略，由内核选择新地址）
        if mremap_flags.contains(MremapFlags::MREMAP_FIXED) {
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
        }

        // 读取旧 VMA 的信息（包括 vm_flags、后备信息、页偏移基址）
        let old_vma = self
            .mappings
            .contains(old_vaddr)
            .ok_or(SystemError::EINVAL)?;
        let (old_region, vm_flags, vm_file, shared_anon, base_pgoff) = {
            let g = old_vma.lock();
            let region = *g.region();
            let flags = *g.vm_flags();
            let vma_start = region.start();
            let off_pages =
                (old_vaddr.data().saturating_sub(vma_start.data())) >> MMArch::PAGE_SHIFT;
            let base = g
                .backing_page_offset()
                .unwrap_or(0)
                .saturating_add(off_pages);
            (region, flags, g.vm_file(), g.shared_anon.clone(), base)
        };

        // 初始化内存区域保护标志
        let prot_flags: ProtFlags = vm_flags.into();

        // 构造目标映射 flags：mremap 需要保留 shared/private 语义，并区分 anon/file。
        let mut map_flags: MapFlags = vm_flags.into();
        if map_flags.contains(MapFlags::MAP_SHARED) {
            // ok
        } else {
            map_flags |= MapFlags::MAP_PRIVATE;
        }
        if vm_file.is_none() {
            map_flags |= MapFlags::MAP_ANONYMOUS;
        }

        // 取消新内存区域的原映射
        if mremap_flags.contains(MremapFlags::MREMAP_FIXED) {
            map_flags |= MapFlags::MAP_FIXED;
            let start_page = VirtPageFrame::new(new_vaddr);
            let page_count = PageFrameCount::from_bytes(new_len).unwrap();
            self.munmap(start_page, page_count)?;
        }

        // 是否允许移动（Linux: 只有 MAYMOVE / FIXED 才能移动）
        let can_move = mremap_flags.contains(MremapFlags::MREMAP_MAYMOVE)
            || mremap_flags.contains(MremapFlags::MREMAP_FIXED);

        // Linux: old_len==0 表示“复制/重复映射”共享区域（DOS-emu legacy）。
        // - 仅允许对共享映射进行
        // - 没有 MAYMOVE/FIXED 时返回 ENOMEM
        if old_len == 0 {
            if !vm_flags.intersects(VmFlags::VM_SHARED | VmFlags::VM_MAYSHARE) {
                return Err(SystemError::EINVAL);
            }
            if !can_move {
                return Err(SystemError::ENOMEM);
            }
        }

        // 不允许移动时，只能尝试原地扩展。
        if !can_move {
            if new_len <= old_len {
                return Ok(old_vaddr);
            }

            // 仅支持从 VMA 起始地址扩展整个 VMA 的常见场景（符合 gVisor 测例）。
            if old_vaddr != old_region.start() || old_len != old_region.size() {
                return Err(SystemError::ENOMEM);
            }

            let grow = new_len - old_len;
            let grow_region = VirtRegion::new(old_vaddr + old_len, grow);
            if self.mappings.conflicts(grow_region).next().is_some() {
                return Err(SystemError::ENOMEM);
            }

            let removed = self
                .mappings
                .remove_vma(&old_region)
                .ok_or(SystemError::EINVAL)?;
            removed.lock().set_region_size(new_len);
            self.mappings.insert_vma(removed);
            return Ok(old_vaddr);
        }

        // 需要创建一个新映射并迁移（FIXED 或 MAYMOVE）。
        // 注意：必须避免在持有地址空间写锁时触碰用户地址（会触发缺页递归死锁）。
        // Linux 的 mremap 通过移动/复制页表项实现，而不是字节拷贝。

        let new_region: VirtRegion = if mremap_flags.contains(MremapFlags::MREMAP_FIXED) {
            VirtRegion::new(new_vaddr, new_len)
        } else {
            self.mappings
                .find_free(self.mmap_min, new_len)
                .ok_or(SystemError::ENOMEM)?
        };

        let entry_flags = EntryFlags::from_prot_flags(prot_flags, true);

        // 创建目标 VMA（初始不映射物理页；存在的页表项会在下面被移动/复制）。
        let new_vma: Arc<LockedVMA> = {
            let vma = LockedVMA::new(VMA::new(
                new_region,
                vm_flags,
                entry_flags,
                vm_file.clone(),
                if vm_file.is_some() || shared_anon.is_some() {
                    Some(base_pgoff)
                } else {
                    None
                },
                false,
            ));
            if let Some(shared) = shared_anon.clone() {
                let mut vg = vma.lock();
                vg.shared_anon = Some(shared);
                vg.backing_pgoff = Some(base_pgoff);
            }
            self.mappings.insert_vma(vma.clone());
            vma
        };

        if let Some(f) = vm_file.as_ref() {
            let _ = f.inode().mmap(
                new_region.start().data(),
                new_len,
                base_pgoff * MMArch::PAGE_SIZE,
            );
        }

        let move_len = core::cmp::min(old_len, new_len);

        // 选择合适的 flusher（与 mmap/munmap 的策略一致）。
        let (mut active, mut inactive);
        let flusher: &mut dyn Flusher<MMArch> = if self.is_current() {
            active = PageFlushAll::new();
            &mut active as &mut dyn Flusher<MMArch>
        } else {
            inactive = InactiveFlusher::new();
            &mut inactive as &mut dyn Flusher<MMArch>
        };

        // 迁移/复制已存在的页表映射。
        // - DONTUNMAP：复制映射（旧映射仍保留）
        // - 否则：移动映射（旧地址解除映射）
        let dontunmap = mremap_flags.contains(MremapFlags::MREMAP_DONTUNMAP) || old_len == 0;
        let mapper = &mut self.user_mapper.utable;
        let old_vma = old_vma.clone();

        let mut page_manager_guard = page_manager_lock();
        let mut off = 0usize;
        while off < move_len {
            let src = old_vaddr + off;
            let dst = new_region.start() + off;
            if let Some((paddr, src_flags)) = mapper.translate(src) {
                if !dontunmap {
                    if let Some((_paddr2, _flags2, flush)) = unsafe { mapper.unmap_phys(src, true) }
                    {
                        flusher.consume(flush);
                    }
                }

                if let Some(flush) = unsafe { mapper.map_phys(dst, paddr, src_flags) } {
                    flusher.consume(flush);
                } else {
                    return Err(SystemError::ENOMEM);
                }

                // 更新物理页的 vma_set
                let page = page_manager_guard.get_unwrap(&paddr);
                let mut pg = page.write_irqsave();
                if !dontunmap {
                    pg.remove_vma(old_vma.as_ref());
                }
                pg.insert_vma(new_vma.clone());
            }
            off += MMArch::PAGE_SIZE;
        }

        // 修复：按照 Linux 语义更新 locked_vm 计数
        // 参考 Linux kernel mm/mremap.c:move_vma() 第714-715行
        // if (vm_flags & VM_LOCKED) {
        //     mm->locked_vm += new_len >> PAGE_SHIFT;
        // }
        //
        // 关键语义：
        // - 移动操作（is_move=true）：调用方会调用 do_munmap 减少 old_len，这里增加 new_len
        // - 原地扩展（is_move=false, new_len > old_len）：调用方不会调用 do_munmap，只增加 delta
        // - MREMAP_DONTUNMAP：调用方不会调用 do_munmap，但新映射需要被计数
        let is_move = new_region.start() != old_vaddr;
        let was_locked =
            vm_flags.contains(VmFlags::VM_LOCKED) || vm_flags.contains(VmFlags::VM_LOCKONFAULT);
        if was_locked {
            let new_pages = new_len >> MMArch::PAGE_SHIFT;
            let dontunmap = mremap_flags.contains(MremapFlags::MREMAP_DONTUNMAP) || old_len == 0;

            if dontunmap {
                // MREMAP_DONTUNMAP：保留旧映射，增加新映射的计数
                self.locked_vm += new_pages;
                // 清除旧 VMA 的 VM_LOCKED 标志（因为页表已移到新映射）
                let mut old_vma_guard = old_vma.lock();
                let current_flags = *old_vma_guard.vm_flags();
                old_vma_guard
                    .set_vm_flags(current_flags & !(VmFlags::VM_LOCKED | VmFlags::VM_LOCKONFAULT));
            } else if is_move {
                // 移动：do_munmap 会减少 old_len，这里增加 new_len
                self.locked_vm += new_pages;
            } else if new_len > old_len {
                // 原地扩展：do_munmap 未被调用，只增加扩展部分
                let old_pages = old_len >> MMArch::PAGE_SHIFT;
                self.locked_vm += new_pages - old_pages;
            } else if new_len < old_len {
                // 原地收缩：do_munmap 未被调用，需要减少收缩部分
                let old_pages = old_len >> MMArch::PAGE_SHIFT;
                self.locked_vm -= old_pages - new_pages;
            }
        }

        Ok(new_region.start())
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
        defer!({
            compiler_fence(Ordering::SeqCst);
        });

        let to_unmap = VirtRegion::new(start_page.virt_address(), page_count.bytes());
        let mut flusher: PageFlushAll<MMArch> = PageFlushAll::new();

        let regions: Vec<Arc<LockedVMA>> = self.mappings.conflicts(to_unmap).collect::<Vec<_>>();
        // 参考 Linux 内核 mm/mmap.c:2460, 2507-2508, 2560
        // do_vmi_align_munmap() 会累加被移除的 VM_LOCKED 页面数，然后在最后减少 mm->locked_vm
        let mut locked_vm = 0;

        for r in regions {
            let r_guard = r.lock();
            let was_locked = r_guard.vm_flags().contains(VmFlags::VM_LOCKED)
                || r_guard.vm_flags().contains(VmFlags::VM_LOCKONFAULT);
            let r_region = *r_guard.region();
            drop(r_guard);

            let r = self.mappings.remove_vma(&r_region).unwrap();
            let intersection = r.lock().region().intersect(&to_unmap).unwrap();
            let split_result = r.extract(intersection, &self.user_mapper.utable).unwrap();

            // TODO: 当引入后备页映射后，这里需要增加通知文件的逻辑

            if let Some(before) = split_result.prev {
                // 如果前面有VMA，则需要将前面的VMA重新插入到地址空间的VMA列表中
                self.mappings.insert_vma(before);
            }

            if let Some(after) = split_result.after {
                // 如果后面有VMA，则需要将后面的VMA重新插入到地址空间的VMA列表中
                self.mappings.insert_vma(after);
            }

            // 参考 Linux 内核 mm/mmap.c:2507-2508
            // 累加被解除映射的锁定页面数
            if was_locked {
                let unmap_len = intersection.end().data() - intersection.start().data();
                locked_vm += unmap_len >> MMArch::PAGE_SHIFT;
            }

            r.unmap(&mut self.user_mapper.utable, &mut flusher);
        }

        // 参考 Linux 内核 mm/mmap.c:2560
        // Point of no return 之后减少 locked_vm
        if locked_vm > 0 {
            self.locked_vm -= locked_vm;
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
        // debug!(
        //     "mprotect: start_page: {:?}, page_count: {:?}, prot_flags:{prot_flags:?}",
        //     start_page,
        //     page_count
        // );
        let (mut active, mut inactive);
        let flusher = if self.is_current() {
            active = PageFlushAll::new();
            &mut active as &mut dyn Flusher<MMArch>
        } else {
            inactive = InactiveFlusher::new();
            &mut inactive as &mut dyn Flusher<MMArch>
        };

        let mapper = &mut self.user_mapper.utable;
        let region = VirtRegion::new(start_page.virt_address(), page_count.bytes());
        // debug!("mprotect: region: {:?}", region);

        let regions = self.mappings.conflicts(region).collect::<Vec<_>>();
        // debug!("mprotect: regions: {:?}", regions);

        for r in regions {
            // debug!("mprotect: r: {:?}", r);
            let r = *r.lock().region();
            let r = self.mappings.remove_vma(&r).unwrap();

            let intersection = r.lock().region().intersect(&region).unwrap();
            let split_result = r
                .extract(intersection, mapper)
                .expect("Failed to extract VMA");

            if let Some(before) = split_result.prev {
                self.mappings.insert_vma(before);
            }
            if let Some(after) = split_result.after {
                self.mappings.insert_vma(after);
            }

            let mut r_guard = r.lock();
            // 如果VMA的保护标志不允许指定的修改，则返回错误
            if !r_guard.can_have_flags(prot_flags) {
                drop(r_guard);
                self.mappings.insert_vma(r.clone());
                return Err(SystemError::EACCES);
            }
            r_guard.set_vm_flags(VmFlags::from(prot_flags));

            let new_flags: EntryFlags<MMArch> = r_guard
                .flags()
                .set_execute(prot_flags.contains(ProtFlags::PROT_EXEC))
                .set_write(prot_flags.contains(ProtFlags::PROT_WRITE));

            r_guard.remap(new_flags, mapper, &mut *flusher)?;
            drop(r_guard);
            self.mappings.insert_vma(r);
        }

        return Ok(());
    }

    pub fn mincore(
        &self,
        start_page: VirtPageFrame,
        page_count: PageFrameCount,
        vec: &mut [u8],
    ) -> Result<(), SystemError> {
        let mapper = &self.user_mapper.utable;

        if self.mappings.contains(start_page.virt_address()).is_none() {
            return Err(SystemError::ENOMEM);
        }

        let mut last_vaddr = start_page.virt_address();
        let region = VirtRegion::new(start_page.virt_address(), page_count.bytes());
        let mut vmas = self.mappings.conflicts(region).collect::<Vec<_>>();
        // 为保证与地址连续性的判断正确，这里按起始地址升序遍历
        vmas.sort_by_key(|v| v.lock().region().start().data());
        let mut offset = 0;
        for v in vmas {
            let region = *v.lock().region();
            // 保证相邻的两个vma连续
            if region.start() != last_vaddr && last_vaddr != start_page.virt_address() {
                return Err(SystemError::ENOMEM);
            }
            let start_vaddr = last_vaddr;
            let end_vaddr = core::cmp::min(region.end(), start_vaddr + page_count.bytes());
            v.do_mincore(mapper, vec, start_vaddr, end_vaddr, offset)?;
            let page_count_this_vma = (end_vaddr - start_vaddr) >> MMArch::PAGE_SHIFT;
            offset += page_count_this_vma;
            last_vaddr = end_vaddr;
        }

        // 校验覆盖完整性：若末尾未覆盖到请求范围，则返回 ENOMEM
        if last_vaddr != region.end() {
            return Err(SystemError::ENOMEM);
        }

        return Ok(());
    }
    pub fn madvise(
        &mut self,
        start_page: VirtPageFrame,
        page_count: PageFrameCount,
        behavior: MadvFlags,
    ) -> Result<(), SystemError> {
        let (mut active, mut inactive);
        let flusher = if self.is_current() {
            active = PageFlushAll::new();
            &mut active as &mut dyn Flusher<MMArch>
        } else {
            inactive = InactiveFlusher::new();
            &mut inactive as &mut dyn Flusher<MMArch>
        };

        let mapper = &mut self.user_mapper.utable;

        let region = VirtRegion::new(start_page.virt_address(), page_count.bytes());
        let regions = self.mappings.conflicts(region).collect::<Vec<_>>();

        for r in regions {
            let r = *r.lock().region();
            let r = self.mappings.remove_vma(&r).unwrap();

            let intersection = r.lock().region().intersect(&region).unwrap();
            let split_result = r
                .extract(intersection, mapper)
                .expect("Failed to extract VMA");

            if let Some(before) = split_result.prev {
                self.mappings.insert_vma(before);
            }
            if let Some(after) = split_result.after {
                self.mappings.insert_vma(after);
            }
            r.do_madvise(behavior, mapper, &mut *flusher)?;
            self.mappings.insert_vma(r);
        }
        Ok(())
    }

    /// 取消与指定 inode 关联的文件映射的页表项，保留 VMA 以便后续访问触发缺页并按最新文件大小处理
    pub fn zap_file_mappings(&mut self, inode_id: InodeId) -> Result<(), SystemError> {
        let mut targets: Vec<Arc<LockedVMA>> = Vec::new();
        for vma in self.mappings.iter_vmas() {
            let guard = vma.lock();
            if let Some(file) = guard.vm_file() {
                if file.inode().metadata()?.inode_id == inode_id {
                    targets.push(vma.clone());
                }
            }
        }

        let mut flusher: PageFlushAll<MMArch> = PageFlushAll::new();
        for vma in targets {
            vma.unmap(&mut self.user_mapper.utable, &mut flusher);
        }
        Ok(())
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
            if vma.mapped() {
                vma.unmap(&mut self.user_mapper.utable, &mut flusher);
            }
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

        // 软限制：RLIMIT_DATA
        let rlim = ProcessManager::current_pcb()
            .get_rlimit(RLimitID::Data)
            .rlim_cur as usize;
        if rlim != usize::MAX {
            let desired = new_brk.data().saturating_sub(self.brk_start.data());
            if desired > rlim {
                return Err(SystemError::ENOMEM);
            }
        }

        let old_brk = self.brk;

        if new_brk > self.brk {
            let len = new_brk - self.brk;
            let prot_flags = ProtFlags::PROT_READ | ProtFlags::PROT_WRITE;
            let map_flags = MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS | MapFlags::MAP_FIXED;
            self.map_anonymous(old_brk, len, prot_flags, map_flags, true, false)?;

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
            self.brk - incr.unsigned_abs()
        };

        let new_brk = VirtAddr::new(page_align_up(new_brk.data()));

        let rlim = ProcessManager::current_pcb()
            .get_rlimit(RLimitID::Data)
            .rlim_cur as usize;
        if rlim != usize::MAX {
            let desired = new_brk.data().saturating_sub(self.brk_start.data());
            if desired > rlim {
                return Err(SystemError::ENOMEM);
            }
        }

        return self.set_brk(new_brk);
    }

    pub fn find_free_at(
        &mut self,
        min_vaddr: VirtAddr,
        vaddr: VirtAddr,
        size: usize,
        flags: MapFlags,
    ) -> Result<VirtRegion, SystemError> {
        // 如果没有指定地址，那么就在当前进程的地址空间中寻找一个空闲的虚拟内存范围。
        if vaddr == VirtAddr::new(0) {
            return self
                .mappings
                .find_free(min_vaddr, size)
                .ok_or(SystemError::ENOMEM);
        }

        // 如果指定了地址，那么就检查指定的地址是否可用。
        let requested = VirtRegion::new(vaddr, size);

        if requested.end() >= MMArch::USER_END_VADDR || !vaddr.check_aligned(MMArch::PAGE_SIZE) {
            return Err(SystemError::EINVAL);
        }

        let intersect_vma = self.mappings.conflicts(requested).next();
        if let Some(vma) = intersect_vma {
            if flags.contains(MapFlags::MAP_FIXED_NOREPLACE) {
                // 如果指定了 MAP_FIXED_NOREPLACE 标志，由于所指定的地址无法成功建立映射，则放弃映射，不对地址做修正
                return Err(SystemError::EEXIST);
            }

            if flags.contains(MapFlags::MAP_FIXED) {
                // 对已有的VMA进行覆盖
                let intersect_region = vma.lock().region.intersect(&requested).unwrap();
                self.munmap(
                    VirtPageFrame::new(intersect_region.start),
                    PageFrameCount::from_bytes(intersect_region.size).unwrap(),
                )?;
                return Ok(requested);
            }

            // 如果没有指定MAP_FIXED标志，那么就对地址做修正
            let requested = self
                .mappings
                .find_free(min_vaddr, size)
                .ok_or(SystemError::ENOMEM)?;
            return Ok(requested);
        }

        return Ok(requested);
    }

    /// 锁定地址范围
    ///
    /// # 参数
    /// - `start`: 起始虚拟地址
    /// - `len`: 长度（已页对齐）
    /// - `onfault`: 是否延迟锁定
    ///
    /// # Linux 语义
    /// 参考 Linux mm/mlock.c:do_mlock():
    /// - VMA 标志设置是破坏性操作，一旦设置就不会回滚
    /// - 对于不可访问的 VMA（如 PROT_NONE），仍然设置 VM_LOCKED 标志
    /// - 但在页面锁定步骤中，不可访问的 VMA 会被跳过
    /// - 这样可以保持状态一致性，避免 TOCTOU 竞态条件
    ///
    /// # 返回值
    ///
    /// 返回 `Result<bool, SystemError>`，其中 `bool` 表示是否包含不可访问的 VMA：
    /// - `Ok(true)`: 包含不可访问的 VMA（如 PROT_NONE），VMA 标志已设置，但调用方应返回 ENOMEM
    /// - `Ok(false)`: 所有 VMA 均可访问，操作完全成功
    /// - `Err(e)`: 发生错误
    pub fn mlock(
        &mut self,
        start: VirtAddr,
        len: usize,
        onfault: bool,
    ) -> Result<bool, SystemError> {
        // 计算结束地址
        let end = start.data().checked_add(len).ok_or(SystemError::ENOMEM)?;
        let end = VirtAddr::new(end);
        // 构造要设置的标志
        let mut new_flags = VmFlags::VM_LOCKED;
        if onfault {
            new_flags |= VmFlags::VM_LOCKONFAULT;
        }

        // 获取冲突的 VMA 列表
        let region = VirtRegion::new(start, len);
        let vmas: Vec<Arc<LockedVMA>> = self.mappings.conflicts(region).collect();

        let mut newly_locked_pages = 0;
        let mut has_inaccessible_vma = false;

        // 遍历所有 VMA，设置 VM_LOCKED 标志
        // 参考 Linux 语义：VMA 标志设置是破坏性操作，即使后续失败也不回滚
        // 对于不可访问的 VMA（如 PROT_NONE），仍然设置标志，但在后续步骤中跳过页面锁定
        for vma in &vmas {
            let mut guard = vma.lock();
            let current_flags = *guard.vm_flags();
            let vma_start = guard.region().start();
            let vma_end = guard.region().end();

            // 检查 VMA 是否可访问（用于后续返回判断）
            // 仅在非 onfault 模式下才需要检测不可访问的 VMA
            // 注意：这里直接检查 current_flags，不能调用 is_accessible()，
            // 因为 is_accessible() 会尝试获取同一个锁，导致死锁
            if !onfault {
                let vm_access_flags = VmFlags::VM_READ | VmFlags::VM_WRITE | VmFlags::VM_EXEC;
                if !current_flags.intersects(vm_access_flags) {
                    has_inaccessible_vma = true;
                }
            }

            // 检查 VMA 是否已经锁定
            let was_locked = current_flags.contains(VmFlags::VM_LOCKED)
                || current_flags.contains(VmFlags::VM_LOCKONFAULT);

            // 添加锁定标志（总是设置，遵循 Linux 语义）
            guard.set_vm_flags(current_flags | new_flags);
            drop(guard);

            // 如果之前未锁定，则增加计数
            if !was_locked {
                // 计算 VMA 与请求范围的交集
                let lock_start = core::cmp::max(vma_start, start);
                let lock_end = core::cmp::min(vma_end, end);
                let lock_len = lock_end.data() - lock_start.data();
                newly_locked_pages += lock_len >> MMArch::PAGE_SHIFT;
            }
        }

        // 更新 locked_vm
        self.locked_vm += newly_locked_pages;

        // 锁定已映射的页面
        // 对于 onfault 模式，不在此时锁定页面，而是在缺页中断时锁定
        if !onfault {
            unsafe {
                let mapper = PageMapper::current(PageTableKind::User, LockedFrameAllocator);

                for vma in &vmas {
                    // 只锁定可访问的 VMA
                    if !vma.is_accessible() {
                        continue;
                    }

                    let vma_guard = vma.lock();
                    let vma_start = vma_guard.region().start();
                    let vma_end = vma_guard.region().end();
                    drop(vma_guard);

                    // 计算 VMA 与请求范围的交集
                    let lock_start = core::cmp::max(vma_start, start);
                    let lock_end = core::cmp::min(vma_end, end);

                    // 锁定该范围内的已映射页面，不返回真正的错误，当页表项未映射时跳过
                    vma.mlock_vma_pages_range(&mapper, lock_start, lock_end, true);
                }
            }
        }

        Ok(has_inaccessible_vma)
    }

    /// 解锁地址范围
    pub fn munlock(&mut self, start: VirtAddr, len: usize) -> Result<(), SystemError> {
        let end = start.data().checked_add(len).ok_or(SystemError::ENOMEM)?;
        let end = VirtAddr::new(end);

        // 获取冲突的 VMA 列表
        let region = VirtRegion::new(start, len);
        let vmas: Vec<Arc<LockedVMA>> = self.mappings.conflicts(region).collect();

        let mut unlocked_pages = 0;

        unsafe {
            let mapper = PageMapper::current(PageTableKind::User, LockedFrameAllocator);

            for vma in &vmas {
                let vma_guard = vma.lock();
                let vm_flags = *vma_guard.vm_flags();
                let vma_start = vma_guard.region().start();
                let vma_end = vma_guard.region().end();
                drop(vma_guard);

                // 只处理已锁定或 lock on fault 的 VMA
                let was_locked = vm_flags.contains(VmFlags::VM_LOCKED)
                    || vm_flags.contains(VmFlags::VM_LOCKONFAULT);

                if !was_locked {
                    continue;
                }

                // 计算 VMA 与请求范围的交集
                let unlock_start = core::cmp::max(vma_start, start);
                let unlock_end = core::cmp::min(vma_end, end);

                // 解锁该范围内的已映射页面
                vma.mlock_vma_pages_range(&mapper, unlock_start, unlock_end, false);
                // 清除 VMA 的锁定标志
                let mut guard = vma.lock();
                let current_flags = *guard.vm_flags();
                guard.set_vm_flags(current_flags & !(VmFlags::VM_LOCKED | VmFlags::VM_LOCKONFAULT));

                // 计算实际解锁的页面数
                let unlock_len = unlock_end.data() - unlock_start.data();
                unlocked_pages += unlock_len >> MMArch::PAGE_SHIFT;
            }
        }

        // 更新 locked_vm 计数（只减少实际解锁的页面数）
        self.locked_vm -= unlocked_pages;

        Ok(())
    }

    /// 锁定所有内存映射
    pub fn mlockall(&mut self, flags: u32) -> Result<(), SystemError> {
        use crate::mm::syscall::MlockAllFlags;

        let mlock_flags = MlockAllFlags::from_bits(flags).ok_or(SystemError::EINVAL)?;

        // 设置 def_flags（影响未来的映射）
        let mut vm_flags = VmFlags::empty();
        if mlock_flags.contains(MlockAllFlags::MCL_FUTURE) {
            vm_flags |= VmFlags::VM_LOCKED;
            if mlock_flags.contains(MlockAllFlags::MCL_ONFAULT) {
                vm_flags |= VmFlags::VM_LOCKONFAULT;
            }
        }
        self.def_flags = vm_flags;

        // 如果设置了 MCL_CURRENT，锁定当前所有映射
        if mlock_flags.contains(MlockAllFlags::MCL_CURRENT) {
            let mut lock_flags = VmFlags::VM_LOCKED;
            if mlock_flags.contains(MlockAllFlags::MCL_ONFAULT) {
                lock_flags |= VmFlags::VM_LOCKONFAULT;
            }

            // 收集所有 VMA 的引用，以便在设置标志后锁定页面
            let vmas_to_lock: alloc::vec::Vec<(Arc<LockedVMA>, VirtAddr, VirtAddr)> = self
                .mappings
                .vmas
                .iter()
                .filter_map(|vma| {
                    if !vma.is_accessible() {
                        return None;
                    }
                    let vma_guard = vma.lock();
                    let vm_flags = *vma_guard.vm_flags();
                    let region = *vma_guard.region();
                    drop(vma_guard);

                    // 只处理还未设置 VM_LOCKED 的 VMA
                    if !vm_flags.contains(VmFlags::VM_LOCKED) {
                        Some((vma.clone(), region.start(), region.end()))
                    } else {
                        None
                    }
                })
                .collect();

            // 先设置所有 VMA 的标志
            for (vma, _, _) in &vmas_to_lock {
                let mut guard = vma.lock();
                let current_flags = *guard.vm_flags();
                guard.set_vm_flags(current_flags | lock_flags);
            }

            // 然后锁定页面（对于非 onfault 模式）
            // 基于 VMA 大小计算 locked_vm，保持与 mlock() 一致
            let mut total_pages = 0;
            for (_, start, end) in &vmas_to_lock {
                let len = end.data() - start.data();
                total_pages += len >> MMArch::PAGE_SHIFT;
            }
            self.locked_vm += total_pages;

            if !mlock_flags.contains(MlockAllFlags::MCL_ONFAULT) {
                unsafe {
                    let mapper = PageMapper::current(PageTableKind::User, LockedFrameAllocator);
                    for (vma, start, end) in vmas_to_lock {
                        vma.mlock_vma_pages_range(&mapper, start, end, true);
                    }
                }
            }
        }

        Ok(())
    }

    /// 解锁所有内存映射
    pub fn munlockall(&mut self) -> Result<(), SystemError> {
        // 收集所有需要解锁的 VMA
        let vmas_to_unlock: alloc::vec::Vec<(Arc<LockedVMA>, VirtAddr, VirtAddr)> = self
            .mappings
            .vmas
            .iter()
            .filter_map(|vma| {
                let vma_guard = vma.lock();
                let vm_flags = *vma_guard.vm_flags();
                let region = *vma_guard.region();
                drop(vma_guard);

                // 只处理已锁定或 lock on fault 的 VMA
                if vm_flags.contains(VmFlags::VM_LOCKED)
                    || vm_flags.contains(VmFlags::VM_LOCKONFAULT)
                {
                    Some((vma.clone(), region.start(), region.end()))
                } else {
                    None
                }
            })
            .collect();

        unsafe {
            let mapper = PageMapper::current(PageTableKind::User, LockedFrameAllocator);

            // 先解锁所有页面
            for (vma, start, end) in &vmas_to_unlock {
                vma.mlock_vma_pages_range(&mapper, *start, *end, false);
            }
        }

        // 清除 def_flags
        self.def_flags = VmFlags::empty();

        // 遍历所有 VMA，清除锁定标志，并计算要减少的 locked_vm
        // 参考 Linux 语义：locked_vm 统计的是带 VM_LOCKED 标志的 VMA 页面数
        let mut pages_to_subtract = 0;
        for (vma, start, end) in &vmas_to_unlock {
            let mut guard = vma.lock();
            let current_flags = *guard.vm_flags();
            // 只有当前确实有 VM_LOCKED 标志的 VMA 才需要减少计数
            if current_flags.contains(VmFlags::VM_LOCKED) {
                let len = end.data() - start.data();
                pages_to_subtract += len >> MMArch::PAGE_SHIFT;
            }
            guard.set_vm_flags(current_flags & !(VmFlags::VM_LOCKED | VmFlags::VM_LOCKONFAULT));
        }

        // 根据解锁的 VMA 页面数减少 locked_vm（而不是直接设为 0）
        self.locked_vm = self.locked_vm.saturating_sub(pages_to_subtract);

        Ok(())
    }

    pub fn locked_vm(&self) -> usize {
        self.locked_vm
    }

    /// 计算指定范围内已锁定的页面数
    ///
    /// # 参数
    /// - `start`: 起始虚拟地址
    /// - `len`: 长度
    ///
    /// # 返回
    /// 返回指定范围内已锁定的页面数
    ///
    /// # 说明
    /// 参考 Linux: mm/mlock.c:count_mm_mlocked_page_nr()
    /// 用于处理 mlock/mlock2 时，如果请求范围内有部分已经锁定，
    /// 需要从请求的页面数中扣除已锁定的部分。
    pub fn count_mm_mlocked_page_nr(&self, start: VirtAddr, len: usize) -> usize {
        let end = start.data().saturating_add(len);
        let end = VirtAddr::new(core::cmp::min(end, MMArch::USER_END_VADDR.data()));

        let region = VirtRegion::new(start, len);
        let mut count = 0;

        for vma in self.mappings.conflicts(region) {
            let vma_guard = vma.lock();
            let vm_flags = *vma_guard.vm_flags();
            let vma_region = *vma_guard.region();
            drop(vma_guard);

            // 只计算已锁定的 VMA
            if !vm_flags.contains(VmFlags::VM_LOCKED) && !vm_flags.contains(VmFlags::VM_LOCKONFAULT)
            {
                continue;
            }

            // 计算 VMA 与请求范围的交集
            let intersection_start = core::cmp::max(vma_region.start(), start);
            let intersection_end = core::cmp::min(vma_region.end(), end);

            if intersection_end > intersection_start {
                let intersect_len = intersection_end.data() - intersection_start.data();
                count += intersect_len >> MMArch::PAGE_SHIFT;
            }
        }

        count
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

    /// 拷贝用户空间映射
    /// ## 参数
    ///
    /// - `umapper`: 要拷贝的用户空间
    /// - `copy_on_write`: 是否写时复制
    pub unsafe fn clone_from(&mut self, umapper: &mut Self, copy_on_write: bool) {
        self.utable
            .clone_user_mapping(&mut umapper.utable, copy_on_write);
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

    /// 向下寻找距离虚拟地址最近的VMA
    /// ## 参数
    ///
    /// - `vaddr`: 虚拟地址
    ///
    /// ## 返回值
    /// - Some(Arc<LockedVMA>): 虚拟地址所在的或最近的下一个VMA
    /// - None: 未找到VMA
    #[allow(dead_code)]
    pub fn find_nearest(&self, vaddr: VirtAddr) -> Option<Arc<LockedVMA>> {
        let mut nearest: Option<Arc<LockedVMA>> = None;
        for v in self.vmas.iter() {
            let guard = v.lock();
            if guard.region.contains(vaddr) {
                return Some(v.clone());
            }
            // 向下寻找：选择起始地址大于 vaddr 的 VMA 中，起始地址最小的一个（最近的下一个VMA）
            if guard.region.start > vaddr
                && if let Some(ref current) = nearest {
                    guard.region.start < current.lock().region.start
                } else {
                    true
                }
            {
                nearest = Some(v.clone());
            }
        }
        return nearest;
    }

    /// 获取当前进程的地址空间中，与给定虚拟地址范围有重叠的VMA的迭代器。
    pub fn conflicts(&self, request: VirtRegion) -> impl Iterator<Item = Arc<LockedVMA>> + '_ {
        let r = self
            .vmas
            .iter()
            .filter(move |v| v.lock().region.intersect(&request).is_some())
            .cloned();
        return r;
    }

    /// 在当前进程的地址空间中，寻找第一个符合条件的空闲的虚拟内存范围。
    ///
    /// @param min_vaddr 最小的起始地址
    /// @param size 请求的大小
    ///
    /// @return 如果找到了，返回虚拟内存范围，否则返回None
    pub fn find_free(&self, min_vaddr: VirtAddr, req_size: usize) -> Option<VirtRegion> {
        let mut iter = self
            .vm_holes
            .iter()
            .skip_while(|(hole_vaddr, hole_size)| hole_vaddr.add(**hole_size) <= min_vaddr);

        let (hole_vaddr, _hole_size) = iter.find(|(hole_vaddr, hole_size)| {
            // 计算当前空洞的可用大小
            let available_size: usize =
                if hole_vaddr <= &&min_vaddr && min_vaddr <= hole_vaddr.add(**hole_size) {
                    **hole_size - (min_vaddr - **hole_vaddr)
                } else {
                    **hole_size
                };

            req_size <= available_size
        })?;

        // 返回恰好等于请求大小的区域，起始地址取空洞与下限的较大值。
        let region = VirtRegion::new(cmp::max(*hole_vaddr, min_vaddr), req_size);

        return Some(region);
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
        let region = vma.lock().region;
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
    pub fn iter_vmas(&self) -> hashbrown::hash_set::Iter<'_, Arc<LockedVMA>> {
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
pub struct LockedVMA {
    /// 用于计算哈希值，避免总是获取vma锁来计算哈希值
    id: usize,
    vma: Mutex<VMA>,
}

impl core::hash::Hash for LockedVMA {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl PartialEq for LockedVMA {
    fn eq(&self, other: &Self) -> bool {
        self.id.eq(&other.id)
    }
}

impl Eq for LockedVMA {}

#[allow(dead_code)]
impl LockedVMA {
    pub fn new(vma: VMA) -> Arc<Self> {
        let r = Arc::new(Self {
            id: LOCKEDVMA_ID_ALLOCATOR.lock().alloc().unwrap(),
            vma: Mutex::new(vma),
        });
        r.vma.lock().self_ref = Arc::downgrade(&r);
        return r;
    }

    pub fn id(&self) -> usize {
        self.id
    }

    pub fn lock(&self) -> MutexGuard<'_, VMA> {
        return self.vma.lock();
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
        flags: EntryFlags<MMArch>,
        mapper: &mut PageMapper,
        mut flusher: impl Flusher<MMArch>,
    ) -> Result<(), SystemError> {
        let mut guard = self.lock();
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

        // 获取物理页的anon_vma的守卫
        let mut page_manager_guard = page_manager_lock();

        // 获取映射的物理地址
        if let Some((paddr, _flags)) = mapper.translate(guard.region().start()) {
            // 如果是共享页，执行释放操作
            let page = page_manager_guard.get(&paddr).unwrap();
            let _page_guard = page.read_irqsave();
            if let Some(shm_id) = guard.shm_id {
                let ipcns = ProcessManager::current_ipcns();
                let mut shm_manager_guard = ipcns.shm.lock();
                if let Some(kernel_shm) = shm_manager_guard.get_mut(&shm_id) {
                    // 更新最后一次断开连接时间
                    kernel_shm.update_dtim();

                    // 映射计数减少
                    kernel_shm.decrease_count();

                    // 释放shm_id
                    if kernel_shm.map_count() == 0 && kernel_shm.mode().contains(ShmFlags::SHM_DEST)
                    {
                        shm_manager_guard.free_id(&shm_id);
                    }
                }
            }
        }

        for page in guard.region.pages() {
            if mapper.translate(page.virt_address()).is_none() {
                continue;
            }
            let (paddr, _, flush) = unsafe { mapper.unmap_phys(page.virt_address(), true) }
                .expect("Failed to unmap, beacuse of some page is not mapped");

            // 从anon_vma中删除当前VMA
            let page = page_manager_guard.get_unwrap(&paddr);
            let mut page_guard = page.write_irqsave();
            page_guard.remove_vma(self);

            // 如果物理页的vma链表长度为0并且未标记为不可回收，则释放物理页.
            // TODO 后续由lru释放物理页面
            if page_guard.can_deallocate() {
                page_manager_guard.remove_page(&paddr);
            }

            flusher.consume(flush);
        }
        guard.mapped = false;

        // 当vma对应共享文件的写映射时，唤醒脏页回写线程
        if guard.vm_file().is_some()
            && guard
                .vm_flags()
                .contains(VmFlags::VM_SHARED | VmFlags::VM_WRITE)
        {
            crate::mm::page::PageReclaimer::wakeup_claim_thread();
        }
    }

    pub fn mapped(&self) -> bool {
        return self.vma.lock().mapped;
    }

    /// 将当前VMA进行切分，切分成3个VMA，分别是：
    ///
    /// 1. 前面的VMA，如果没有则为None
    /// 2. 中间的VMA，也就是传入的Region
    /// 3. 后面的VMA，如果没有则为None
    pub fn extract(&self, region: VirtRegion, utable: &PageMapper) -> Option<VMASplitResult> {
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
                return Some(VMASplitResult::new(
                    None,
                    guard.self_ref.upgrade().unwrap(),
                    None,
                ));
            }
        }

        let before: Option<Arc<LockedVMA>> = guard.region.before(&region).map(|virt_region| {
            let mut vma: VMA = unsafe { guard.clone() };
            vma.region = virt_region;
            vma.mapped = false;
            // backing_pgoff 保持不变，before VMA 使用原始的offset
            let vma: Arc<LockedVMA> = LockedVMA::new(vma);
            vma
        });

        let after: Option<Arc<LockedVMA>> = guard.region.after(&region).map(|virt_region| {
            let mut vma: VMA = unsafe { guard.clone() };
            vma.region = virt_region;
            vma.mapped = false;
            // after VMA 需要调整backing_pgoff
            // after 区域的起始地址相对于原始VMA起始地址的偏移（以页为单位）
            if let Some(original_pgoff) = vma.backing_pgoff {
                let offset_pages =
                    (virt_region.start() - guard.region.start()) >> MMArch::PAGE_SHIFT;
                vma.backing_pgoff = Some(original_pgoff + offset_pages);
            }
            let vma: Arc<LockedVMA> = LockedVMA::new(vma);
            vma
        });

        // 重新设置before、after这两个VMA里面的物理页的anon_vma
        let mut page_manager_guard = page_manager_lock();
        if let Some(before) = before.clone() {
            let virt_iter = before.lock().region.iter_pages();
            for frame in virt_iter {
                if let Some((paddr, _)) = utable.translate(frame.virt_address()) {
                    let page = page_manager_guard.get_unwrap(&paddr);
                    let mut page_guard = page.write_irqsave();
                    page_guard.insert_vma(before.clone());
                    page_guard.remove_vma(self);
                    before.lock().mapped = true;
                }
            }
        }

        if let Some(after) = after.clone() {
            let virt_iter = after.lock().region.iter_pages();
            for frame in virt_iter {
                if let Some((paddr, _)) = utable.translate(frame.virt_address()) {
                    let page = page_manager_guard.get_unwrap(&paddr);
                    let mut page_guard = page.write_irqsave();
                    page_guard.insert_vma(after.clone());
                    page_guard.remove_vma(self);
                    after.lock().mapped = true;
                }
            }
        }

        // 调整middle VMA的region和backing_pgoff
        let original_start = guard.region.start();
        guard.region = region;
        // middle VMA 需要调整backing_pgoff
        // middle 区域的起始地址相对于原始VMA起始地址的偏移（以页为单位）
        if let Some(original_pgoff) = guard.backing_pgoff {
            let offset_pages = (region.start() - original_start) >> MMArch::PAGE_SHIFT;
            guard.backing_pgoff = Some(original_pgoff + offset_pages);
        }

        return Some(VMASplitResult::new(
            before,
            guard.self_ref.upgrade().unwrap(),
            after,
        ));
    }

    /// 判断VMA是否为外部（非当前进程空间）的VMA
    pub fn is_foreign(&self) -> bool {
        let guard = self.lock();
        if let Some(space) = guard.user_address_space.clone() {
            if let Some(space) = space.upgrade() {
                return AddressSpace::is_current(&space);
            } else {
                return true;
            }
        } else {
            return true;
        }
    }

    /// 判断VMA是否可访问
    pub fn is_accessible(&self) -> bool {
        let guard = self.lock();
        let vm_access_flags: VmFlags = VmFlags::VM_READ | VmFlags::VM_WRITE | VmFlags::VM_EXEC;
        guard.vm_flags().intersects(vm_access_flags)
    }

    /// 判断VMA是否为匿名映射
    pub fn is_anonymous(&self) -> bool {
        let guard = self.lock();
        guard.vm_file.is_none()
    }

    /// 判断VMA是否为大页映射
    pub fn is_hugepage(&self) -> bool {
        //TODO: 实现巨页映射判断逻辑，目前不支持巨页映射
        false
    }
}

impl Drop for LockedVMA {
    fn drop(&mut self) {
        LOCKEDVMA_ID_ALLOCATOR.lock().free(self.id);
    }
}

/// VMA切分结果
#[allow(dead_code)]
pub struct VMASplitResult {
    pub prev: Option<Arc<LockedVMA>>,
    pub middle: Arc<LockedVMA>,
    pub after: Option<Arc<LockedVMA>>,
}

impl VMASplitResult {
    pub fn new(
        prev: Option<Arc<LockedVMA>>,
        middle: Arc<LockedVMA>,
        post: Option<Arc<LockedVMA>>,
    ) -> Self {
        Self {
            prev,
            middle,
            after: post,
        }
    }
}

/// Parameters for physmap operation
#[derive(Debug)]
pub struct PhysmapParams {
    pub phys: PhysPageFrame,
    pub destination: VirtPageFrame,
    pub count: PageFrameCount,
    pub vm_flags: VmFlags,
    pub flags: EntryFlags<MMArch>,
    pub shm_id: Option<ShmId>,
}

/// @brief 虚拟内存区域
#[derive(Debug)]
pub struct VMA {
    /// 虚拟内存区域对应的虚拟地址范围
    region: VirtRegion,
    /// 虚拟内存区域标志
    vm_flags: VmFlags,
    /// VMA内的页帧的标志
    flags: EntryFlags<MMArch>,
    /// VMA内的页帧是否已经映射到页表
    mapped: bool,
    /// VMA所属的用户地址空间
    user_address_space: Option<Weak<AddressSpace>>,
    self_ref: Weak<LockedVMA>,

    vm_file: Option<Arc<File>>,
    /// VMA映射的后备对象(文件/共享匿名)相对于整个后备对象的偏移页数
    backing_pgoff: Option<usize>,

    provider: Provider,
    /// 关联的 SysV SHM 标识（当此 VMA 来自 shmat 时设置）
    shm_id: Option<ShmId>,
    /// 共享匿名映射的稳定身份（用于跨进程共享 futex key）
    pub(crate) shared_anon: Option<Arc<AnonSharedMapping>>,
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

/// 共享匿名映射的稳定身份
#[derive(Debug)]
pub struct AnonSharedMapping {
    pub id: u64,
    /// Fixed backing size in pages, established at creation time.
    /// Linux semantics: mremap() expanding a MAP_SHARED|MAP_ANONYMOUS mapping does not grow the
    /// underlying shmem object; access beyond this size should SIGBUS.
    size_pages: usize,
    // Per-page cache keyed by page index within the backing object; store physical address.
    pages: SpinLock<HashMap<usize, PhysAddr>>,
}

impl AnonSharedMapping {
    fn new_id() -> u64 {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        return NEXT_ID.fetch_add(1, Ordering::Relaxed);
    }

    pub fn new(size_pages: usize) -> Arc<Self> {
        Arc::new(Self {
            id: Self::new_id(),
            size_pages,
            pages: SpinLock::new(HashMap::new()),
        })
    }

    #[inline(always)]
    pub fn size_pages(&self) -> usize {
        self.size_pages
    }

    /// Get or create a shared page for the given offset atomically.
    /// This prevents the double-allocation race when multiple processes fault the same page.
    pub fn get_or_create_page(&self, pgoff: usize) -> Result<Arc<Page>, SystemError> {
        let mut guard = self.pages.lock_irqsave();
        if let Some(paddr) = guard.get(&pgoff).copied() {
            let mut pm = page_manager_lock();
            return Ok(pm.get_unwrap(&paddr));
        }

        // Allocate while holding the map lock to avoid duplicate creations.
        let mut pm = page_manager_lock();
        let mut allocator = LockedFrameAllocator;
        let page = pm.create_one_page(PageType::Normal, PageFlags::empty(), &mut allocator)?;
        // Mark shared-anon pages as unevictable so shrinking/unmapping doesn't drop their contents.
        page.write_irqsave().add_flags(PageFlags::PG_UNEVICTABLE);
        guard.insert(pgoff, page.phys_address());
        Ok(page)
    }
}

impl Drop for AnonSharedMapping {
    fn drop(&mut self) {
        // When the backing object is destroyed, allow cached pages to be freed.
        let pages: alloc::vec::Vec<PhysAddr> = {
            let guard = self.pages.lock_irqsave();
            guard.values().copied().collect()
        };

        let mut pm = page_manager_lock();
        for paddr in pages {
            if let Some(page) = pm.get(&paddr) {
                let mut pg = page.write_irqsave();
                pg.remove_flags(PageFlags::PG_UNEVICTABLE);
                if pg.can_deallocate() {
                    drop(pg);
                    pm.remove_page(&paddr);
                }
            }
        }
    }
}

#[allow(dead_code)]
impl VMA {
    pub fn new(
        region: VirtRegion,
        vm_flags: VmFlags,
        flags: EntryFlags<MMArch>,
        file: Option<Arc<File>>,
        pgoff: Option<usize>,
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
            vm_file: file,
            backing_pgoff: pgoff,
            shm_id: None,
            shared_anon: None,
        }
    }

    pub fn region(&self) -> &VirtRegion {
        return &self.region;
    }

    pub fn vm_flags(&self) -> &VmFlags {
        return &self.vm_flags;
    }

    pub fn vm_file(&self) -> Option<Arc<File>> {
        return self.vm_file.clone();
    }

    pub fn address_space(&self) -> Option<Weak<AddressSpace>> {
        return self.user_address_space.clone();
    }

    pub fn set_vm_flags(&mut self, vm_flags: VmFlags) {
        self.vm_flags = vm_flags;
    }

    pub fn set_region_size(&mut self, new_region_size: usize) {
        self.region.set_size(new_region_size);
    }

    pub fn set_mapped(&mut self, mapped: bool) {
        self.mapped = mapped;
    }

    pub fn set_flags(&mut self) {
        self.flags = MMArch::vm_get_page_prot(self.vm_flags);
    }

    #[inline(always)]
    pub fn set_shm_id(&mut self, shm: Option<ShmId>) {
        self.shm_id = shm;
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
            backing_pgoff: self.backing_pgoff,
            vm_file: self.vm_file.clone(),
            shm_id: self.shm_id,
            shared_anon: self.shared_anon.clone(),
        };
    }

    pub fn clone_info_only(&self) -> Self {
        // 注意：fork 时不应继承 VM_LOCKED 标志
        // 参考 Linux: mlock 不会被 fork 的子进程继承
        let vm_flags = self.vm_flags & !(VmFlags::VM_LOCKED | VmFlags::VM_LOCKONFAULT);

        return Self {
            region: self.region,
            vm_flags,
            flags: self.flags,
            mapped: self.mapped,
            user_address_space: None,
            self_ref: Weak::default(),
            provider: Provider::Allocated,
            backing_pgoff: self.backing_pgoff,
            vm_file: self.vm_file.clone(),
            shm_id: self.shm_id,
            shared_anon: self.shared_anon.clone(),
        };
    }

    #[inline(always)]
    pub fn flags(&self) -> EntryFlags<MMArch> {
        return self.flags;
    }

    #[inline(always)]
    pub fn backing_page_offset(&self) -> Option<usize> {
        return self.backing_pgoff;
    }

    pub fn pages(&self) -> VirtPageFrameIter {
        return VirtPageFrameIter::new(
            VirtPageFrame::new(self.region.start()),
            VirtPageFrame::new(self.region.end()),
        );
    }

    pub fn remap(
        &mut self,
        flags: EntryFlags<MMArch>,
        mapper: &mut PageMapper,
        mut flusher: impl Flusher<MMArch>,
    ) -> Result<(), SystemError> {
        for page in self.region.pages() {
            // debug!("remap page {:?}", page.virt_address());
            if mapper.translate(page.virt_address()).is_some() {
                let r = unsafe {
                    mapper
                        .remap(page.virt_address(), flags)
                        .expect("Failed to remap")
                };
                flusher.consume(r);
            }
            // debug!("consume page {:?}", page.virt_address());
            // debug!("remap page {:?} done", page.virt_address());
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

        #[allow(clippy::unneeded_struct_pattern)]
        match self.provider {
            Provider::Allocated { .. } => true,

            #[allow(unreachable_patterns)]
            _ => is_downgrade,
        }
    }

    /// 把物理地址映射到虚拟地址
    ///
    /// @param params 物理映射参数
    /// @param mapper 页表映射器
    /// @param flusher 页表项刷新器
    ///
    /// @return 返回映射后的虚拟内存区域
    pub fn physmap(
        params: PhysmapParams,
        mapper: &mut PageMapper,
        mut flusher: impl Flusher<MMArch>,
    ) -> Result<Arc<LockedVMA>, SystemError> {
        let mut cur_phy = params.phys;
        let mut cur_dest = params.destination;

        for _ in 0..params.count.data() {
            // 将物理页帧映射到虚拟页帧
            let r = unsafe {
                mapper.map_phys(
                    cur_dest.virt_address(),
                    cur_phy.phys_address(),
                    params.flags,
                )
            }
            .expect("Failed to map phys, may be OOM error");

            // todo: 增加OOM处理

            // 刷新TLB
            flusher.consume(r);

            cur_phy = cur_phy.next();
            cur_dest = cur_dest.next();
        }

        let r: Arc<LockedVMA> = LockedVMA::new(VMA::new(
            VirtRegion::new(
                params.destination.virt_address(),
                params.count.data() * MMArch::PAGE_SIZE,
            ),
            params.vm_flags,
            params.flags,
            None,
            None,
            true,
        ));
        if let Some(id) = params.shm_id {
            r.lock().set_shm_id(Some(id));
        }

        // 将VMA加入到anon_vma中
        let mut page_manager_guard = page_manager_lock();
        cur_phy = params.phys;
        for _ in 0..params.count.data() {
            let paddr = cur_phy.phys_address();
            let page = page_manager_guard.get_unwrap(&paddr);
            page.write_irqsave().insert_vma(r.clone());
            cur_phy = cur_phy.next();
        }

        return Ok(r);
    }

    /// 从页分配器中分配一些物理页，并把它们映射到指定的虚拟地址，然后创建VMA
    /// ## 参数
    ///
    /// - `destination`: 要映射到的虚拟地址
    /// - `page_count`: 要映射的页帧数量
    /// - `vm_flags`: VMA标志位
    /// - `flags`: 页面标志位
    /// - `mapper`: 页表映射器
    /// - `flusher`: 页表项刷新器
    /// - `file`: 映射文件
    /// - `pgoff`: 返回映射后的虚拟内存区域
    ///
    /// ## 返回值
    /// - 页面错误处理信息标志
    #[allow(clippy::too_many_arguments)]
    pub fn zeroed(
        destination: VirtPageFrame,
        page_count: PageFrameCount,
        vm_flags: VmFlags,
        flags: EntryFlags<MMArch>,
        mapper: &mut PageMapper,
        mut flusher: impl Flusher<MMArch>,
        file: Option<Arc<File>>,
        pgoff: Option<usize>,
    ) -> Result<Arc<LockedVMA>, SystemError> {
        let mut cur_dest: VirtPageFrame = destination;
        // debug!(
        //     "VMA::zeroed: page_count = {:?}, destination={destination:?}",
        //     page_count
        // );
        for _ in 0..page_count.data() {
            // debug!(
            //     "VMA::zeroed: cur_dest={cur_dest:?}, vaddr = {:?}",
            //     cur_dest.virt_address()
            // );
            let r = unsafe { mapper.map(cur_dest.virt_address(), flags) }
                .expect("Failed to map zero, may be OOM error");
            // todo: 增加OOM处理

            // 稍后再刷新TLB，这里取消刷新
            flusher.consume(r);
            cur_dest = cur_dest.next();
        }
        let r = LockedVMA::new(VMA::new(
            VirtRegion::new(
                destination.virt_address(),
                page_count.data() * MMArch::PAGE_SIZE,
            ),
            vm_flags,
            flags,
            file,
            pgoff,
            true,
        ));
        drop(flusher);
        // debug!("VMA::zeroed: flusher dropped");

        // 清空这些内存并将VMA加入到anon_vma中
        let mut page_manager_guard = page_manager_lock();
        let virt_iter: VirtPageFrameIter =
            VirtPageFrameIter::new(destination, destination.add(page_count));
        for frame in virt_iter {
            let paddr = mapper.translate(frame.virt_address()).unwrap().0;

            // 将VMA加入到anon_vma
            let page = page_manager_guard.get_unwrap(&paddr);
            page.write_irqsave().insert_vma(r.clone());
        }
        // debug!("VMA::zeroed: done");
        return Ok(r);
    }

    pub fn page_address(&self, index: usize) -> Result<VirtAddr, SystemError> {
        if index >= self.backing_pgoff.unwrap() {
            let address =
                self.region.start + ((index - self.backing_pgoff.unwrap()) << MMArch::PAGE_SHIFT);
            if address <= self.region.end() {
                return Ok(address);
            }
        }
        return Err(SystemError::EFAULT);
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
        Some(self.cmp(other))
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
    /// 用户自定义的栈大小限制
    max_limit: usize,
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

        // Layout
        // -------------- high->sp
        // | stack pages|
        // |------------|
        // | not mapped |
        // -------------- low

        let prot_flags = ProtFlags::PROT_READ | ProtFlags::PROT_WRITE | ProtFlags::PROT_EXEC;
        let map_flags = MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS | MapFlags::MAP_GROWSDOWN;

        let stack_size = page_align_up(stack_size);

        // log::info!(
        //     "UserStack stack_range: {:#x} - {:#x}",
        //     stack_bottom.data() - stack_size,
        //     stack_bottom.data()
        // );

        vm.map_anonymous(
            stack_bottom - stack_size,
            stack_size,
            prot_flags,
            map_flags,
            false,
            false,
        )?;

        let max_limit = core::cmp::max(Self::DEFAULT_USER_STACK_SIZE, stack_size);

        let user_stack = UserStack {
            stack_bottom,
            mapped_size: stack_size,
            current_sp: stack_bottom,
            max_limit,
        };

        return Ok(user_stack);
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
            max_limit: self.max_limit,
        };
    }

    /// 获取当前用户栈的大小（不包括保护页）
    pub fn stack_size(&self) -> usize {
        return self.mapped_size;
    }

    /// 设置当前用户栈的最大大小
    pub fn set_max_limit(&mut self, max_limit: usize) {
        self.max_limit = max_limit;
    }

    /// 获取当前用户栈的最大大小限制
    pub fn max_limit(&self) -> usize {
        self.max_limit
    }
}
