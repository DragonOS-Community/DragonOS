use bitfield_struct::bitfield;
use x86::msr;
use crate::mm::PhysAddr;
use crate::{syscall::SystemError, arch::mm::LockedFrameAllocator};
use crate::mm::allocator::page_frame::{FrameAllocator, PageFrameCount};
use super::vcpu::PAGE_SIZE;

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
fn check_ept_features() -> Result<(), SystemError> {
    const MTRR_ENABLE_BIT: u64 = 1 << 11;
    let ia32_mtrr_def_type = unsafe { msr::rdmsr(msr::IA32_MTRR_DEF_TYPE) };
    if (ia32_mtrr_def_type & MTRR_ENABLE_BIT) == 0 {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    Ok(())
}
