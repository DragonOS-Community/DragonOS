use core::
    sync::atomic::{compiler_fence, AtomicUsize, Ordering};

use bitfield_struct::bitfield;
use x86::msr;
use crate::arch::MMArch;
use crate::arch::mm::PageMapper;
use crate::mm::page::PageFlags;
use crate::mm::{PhysAddr, PageTableKind, VirtAddr};
use crate::smp::core::smp_get_processor_id;
use crate::{syscall::SystemError, arch::mm::LockedFrameAllocator};
use crate::mm::allocator::page_frame::{FrameAllocator, PageFrameCount};
use super::vcpu::PAGE_SIZE;

pub const PT64_LEVEL_BITS: u64 = 9;

pub const PT_PRESENT_MASK: u64 = (1 as u64) << 0;
pub const PT_WRITABLE_MASK: u64 = (1 as u64) << 1;
pub const PT_USER_MASK: u64 = (1 as u64) << 2;
pub const PT_PWT_MASK: u64 = (1 as u64) << 3;
pub const PT_PCD_MASK: u64 = (1 as u64) << 4;


//https://docs.rs/bitfield-struct/latest/bitfield_struct/
#[bitfield(u64)]
struct EPTP {
    #[bits(3)]
    memory_type: usize, // bit 2:0 (0 = Uncacheable (UC) - 6 = Write - back(WB))
    #[bits(3)]
    page_walk_length: usize, // bit 5:3 (This value is 1 less than the EPT page-walk length) 
    enable_accessed_and_dirty_flags: bool, // bit 6  (Setting this control to 1 enables accessed and dirty flags for EPT)
    #[bits(5)]
    reserved1: usize, // bit 11:7 
    #[bits(36)]
    pml4_address: usize,
    #[bits(16)]
    reserved2: usize,
}
#[bitfield(u64)]
struct EPT_PML4E {
    /// Booleans are 1 bit large
    read: bool,
    write: bool,
    execute: bool,
    #[bits(5)]
    reseverd1: usize,
    accessed: bool,
    ignored1: bool,
    execute_for_user: bool,
    ignored2: bool,
    #[bits(36)]
    physical_address: usize,
    #[bits(4)]
    reseverd2: usize,
    #[bits(12)]
    ignored3: usize,
}

#[bitfield(u64)]
struct EPT_PDPTE {
    /// Booleans are 1 bit large
    read: bool,
    write: bool,
    execute: bool,
    #[bits(5)]
    reseverd1: usize,
    accessed: bool,
    ignored1: bool,
    execute_for_user: bool,
    ignored2: bool,
    #[bits(36)]
    physical_address: usize,
    #[bits(4)]
    reseverd2: usize,
    #[bits(12)]
    ignored3: usize,
}

#[bitfield(u64)]
struct EPT_PDE {
    read: bool, // bit 0
    write: bool, // bit 1
    execute: bool, // bit 2
    #[bits(5)]
    reseverd1: usize, // bit 7:3 (Must be Zero)
    accessed: bool,   // bit 8
    ignored1: bool,   // bit 9
    execute_for_user: bool, // bit 10
    ignored2: bool, // bit 11
    #[bits(36)] 
    physical_address: usize, // bit (N-1):12 or Page-Frame-Number
    #[bits(4)]
    reseverd2: usize, // bit 51:N
    #[bits(12)]
    ignored3: usize,  // bit 63:52
}

#[bitfield(u64)]
struct EPT_PTE {
    read: bool, // bit 0
    write: bool, // bit 1
    execute: bool, // bit 2
    #[bits(3)]
    ept_memory_type: usize, // bit 5:3 (EPT Memory type)
    ignore_pat: bool, // bit 6
    ignored1: bool,   // bit 7
    accessed: bool,   // bit 8   
    dirty: bool,      // bit 9
    execute_for_user: bool, // bit 10
    ignored2: bool, // bit 11
    #[bits(36)]
    physical_address: usize, // bit (N-1):12 or Page-Frame-Number
    #[bits(4)]
    reseverd2: usize, // bit 51:N
    #[bits(11)]
    ignored3: usize,  // bit 62:52
    supress_ve: bool, // bit 63
}

fn initializeEptp(num_pages: usize) -> Result<*mut EPTP, SystemError>{
    let page_frame_count = PageFrameCount::new(1);
    let (mut paddr, _) = unsafe {LockedFrameAllocator.allocate(page_frame_count).unwrap()};
    let eptp: *mut EPTP = paddr.data() as *mut EPTP;
    // TODO: how to zero out the memory
    unsafe {
        match LockedFrameAllocator.allocate(page_frame_count) {
            Some(data) => {
                paddr = data.0;
            },
            None =>{
                LockedFrameAllocator.free(PhysAddr::new(eptp as usize), page_frame_count);
                return Err(SystemError::ENOMEM);
            }
        }
    };
    let pept_pml4 = paddr.data() as *mut EPT_PML4E;

    unsafe {
        match LockedFrameAllocator.allocate(page_frame_count) {
            Some(data) => {
                paddr = data.0;
            },
            None =>{
                LockedFrameAllocator.free(PhysAddr::new(eptp as usize), page_frame_count);
                LockedFrameAllocator.free(PhysAddr::new(pept_pml4 as usize), page_frame_count);
                return Err(SystemError::ENOMEM);
            }
        }
    };
    let pept_pdpt = paddr.data() as *mut EPT_PDPTE;

    unsafe {
        match LockedFrameAllocator.allocate(page_frame_count) {
            Some(data) => {
                paddr = data.0;
            },
            None =>{
                LockedFrameAllocator.free(PhysAddr::new(eptp as usize), page_frame_count);
                LockedFrameAllocator.free(PhysAddr::new(pept_pml4 as usize), page_frame_count);
                LockedFrameAllocator.free(PhysAddr::new(pept_pdpt as usize), page_frame_count);
                return Err(SystemError::ENOMEM);
            }
        }
    };
    let pept_pd = paddr.data() as *mut EPT_PDE;

    unsafe {
        match LockedFrameAllocator.allocate(page_frame_count) {
            Some(data) => {
                paddr = data.0;
            },
            None =>{
                LockedFrameAllocator.free(PhysAddr::new(eptp as usize), page_frame_count);
                LockedFrameAllocator.free(PhysAddr::new(pept_pml4 as usize), page_frame_count);
                LockedFrameAllocator.free(PhysAddr::new(pept_pdpt as usize), page_frame_count);
                LockedFrameAllocator.free(PhysAddr::new(pept_pd as usize), page_frame_count);
                return Err(SystemError::ENOMEM);
            }
        }
    };
    let pept_pt = paddr.data() as *mut EPT_PTE;

    //
    // Setup PT by allocating two pages Continuously
    // We allocate two pages because we need 1 page for our RIP to start and 1 page for RSP 1 + 1 = 2
    //
    let page_frame_count = PageFrameCount::new(num_pages);
    let (paddr, _) = unsafe {LockedFrameAllocator.allocate(page_frame_count).unwrap()};
    for i in 0..num_pages {
        unsafe {
            (*pept_pt.offset(i as isize)).set_accessed(false);
            (*pept_pt.offset(i as isize)).set_dirty(false);
            (*pept_pt.offset(i as isize)).set_ept_memory_type(6); //  either 0 (for uncached memory) or 6 (writeback) memory
            (*pept_pt.offset(i as isize)).set_execute(true);
            (*pept_pt.offset(i as isize)).set_execute_for_user(false);
            (*pept_pt.offset(i as isize)).set_ignore_pat(false);
            (*pept_pt.offset(i as isize)).set_physical_address(((paddr.data()+ i * PAGE_SIZE) / PAGE_SIZE) as usize);
            (*pept_pt.offset(i as isize)).set_read(true);
            (*pept_pt.offset(i as isize)).set_write(true);
            (*pept_pt.offset(i as isize)).set_supress_ve(false);
        }
    }
    //
    // Setting up PDE
    //
    unsafe {
        (*pept_pd).set_accessed(false);
        (*pept_pd).set_execute(true);
        (*pept_pd).set_execute_for_user(false);
        (*pept_pd).set_ignored1(false);
        (*pept_pd).set_ignored2(false);
        (*pept_pd).set_ignored3(0);
        (*pept_pd).set_physical_address(pept_pt as usize / PAGE_SIZE);
        (*pept_pd).set_read(true);
        (*pept_pd).set_write(true);
        (*pept_pd).set_reseverd1(0);
        (*pept_pd).set_reseverd2(0);
    }
    
    //
    // Setting up PDPTE
    //
    unsafe {
        (*pept_pdpt).set_accessed(false);
        (*pept_pdpt).set_execute(true);
        (*pept_pdpt).set_execute_for_user(false);
        (*pept_pdpt).set_ignored1(false);
        (*pept_pdpt).set_ignored2(false);
        (*pept_pdpt).set_ignored3(0);
        (*pept_pdpt).set_physical_address(pept_pd as usize / PAGE_SIZE);
        (*pept_pdpt).set_read(true);
        (*pept_pdpt).set_write(true);
        (*pept_pdpt).set_reseverd1(0);
        (*pept_pdpt).set_reseverd2(0);
    }

    //
    // Setting up PML4E
    //
    unsafe {
        (*pept_pml4).set_accessed(false);
        (*pept_pml4).set_execute(true);
        (*pept_pml4).set_execute_for_user(false);
        (*pept_pml4).set_ignored1(false);
        (*pept_pml4).set_ignored2(false);
        (*pept_pml4).set_ignored3(0);
        (*pept_pml4).set_physical_address(pept_pdpt as usize / PAGE_SIZE);
        (*pept_pml4).set_read(true);
        (*pept_pml4).set_write(true);
        (*pept_pml4).set_reseverd1(0);
        (*pept_pml4).set_reseverd2(0);
    }

    //
    // Setting up EPTP
    //
    unsafe {
        (*eptp).set_enable_accessed_and_dirty_flags(true);
        (*eptp).set_memory_type(6);
        (*eptp).set_page_walk_length(3);
        (*eptp).set_pml4_address(pept_pml4 as usize / PAGE_SIZE);
        (*eptp).set_reserved1(0);
        (*eptp).set_reserved2(0);
    }

    Ok(eptp)
}


/// Check if MTRR is supported
pub fn check_ept_features() -> Result<(), SystemError> {
    const MTRR_ENABLE_BIT: u64 = 1 << 11;
    let ia32_mtrr_def_type = unsafe { msr::rdmsr(msr::IA32_MTRR_DEF_TYPE) };
    if (ia32_mtrr_def_type & MTRR_ENABLE_BIT) == 0 {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    Ok(())
}

pub fn ept_build_mtrr_map() -> Result<(), SystemError> {
    let ia32_mtrr_cap = unsafe { msr::rdmsr(msr::IA32_MTRRCAP) };
    Ok(())
}
/// 标志当前没有处理器持有内核映射器的锁
/// 之所以需要这个标志，是因为AtomicUsize::new(0)会把0当作一个处理器的id
const EPT_MAPPER_NO_PROCESSOR: usize = !0;
/// 当前持有内核映射器锁的处理器
static EPT_MAPPER_LOCK_OWNER: AtomicUsize = AtomicUsize::new(EPT_MAPPER_NO_PROCESSOR);
/// 内核映射器的锁计数器
static EPT_MAPPER_LOCK_COUNT: AtomicUsize = AtomicUsize::new(0);

pub struct EptMapper{
    /// EPT页表映射器
    mapper: PageMapper,
    /// 标记当前映射器是否为只读
    readonly: bool,
    // EPT页表根地址
    // root_hpa: PhysAddr,
}

impl EptMapper {
    fn lock_cpu(cpuid: usize, mapper: PageMapper) -> Self {
        loop {
            match EPT_MAPPER_LOCK_OWNER.compare_exchange_weak(
                EPT_MAPPER_NO_PROCESSOR,
                cpuid,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                // 当前处理器已经持有了锁
                Err(id) if id == cpuid => break,
                // either CAS failed, or some other hardware thread holds the lock
                Err(_) => core::hint::spin_loop(),
            }
        }

        let prev_count = EPT_MAPPER_LOCK_COUNT.fetch_add(1, Ordering::Relaxed);
        compiler_fence(Ordering::Acquire);

        // 本地核心已经持有过锁，因此标记当前加锁获得的映射器为只读
        let readonly = prev_count > 0;

        return Self { mapper, readonly };
    }

    /// @brief 锁定内核映射器, 并返回一个内核映射器对象
    #[inline(always)]
    pub fn lock() -> Self {
        let cpuid = smp_get_processor_id() as usize;
        let mapper = unsafe { PageMapper::current(PageTableKind::EPT, LockedFrameAllocator) };
        return Self::lock_cpu(cpuid, mapper);
    }
    
    /// 映射guest physical addr(gpa)到指定的host physical addr(hpa)。
    ///
    /// ## 参数
    ///
    /// - `gpa`: 要映射的guest physical addr
    /// - `hpa`: 要映射的host physical addr
    /// - `flags`: 页面标志
    ///
    /// ## 返回
    ///
    /// - 成功：返回Ok(())
    /// - 失败： 如果当前映射器为只读，则返回EAGAIN_OR_EWOULDBLOCK
    pub unsafe fn walk(
        &mut self,
        gpa: u64,
        hpa: u64,
        flags: PageFlags<MMArch>,
    ) -> Result<(), SystemError> {
        if self.readonly {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }
        self.mapper.map_phys(VirtAddr::new(gpa as usize), PhysAddr::new(hpa as usize), flags).unwrap();
        return Ok(());
    }

    // fn get_ept_index(addr: u64, level: usize) -> u64 {
    //     let pt64_level_shift = PAGE_SHIFT + (level - 1) * PT64_LEVEL_BITS;
    //     (addr >> pt64_level_shift) & ((1 << PT64_LEVEL_BITS) - 1)
    // }
}