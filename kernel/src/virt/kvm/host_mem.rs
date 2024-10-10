use system_error::SystemError;

use super::{vcpu::Vcpu, vm};
use crate::{
    kdebug,
    mm::{kernel_mapper::KernelMapper, page::PageFlags, VirtAddr},
};

/*
 * Address types:
 *
 *  gva - guest virtual address
 *  gpa - guest physical address
 *  gfn - guest frame number
 *  hva - host virtual address
 *  hpa - host physical address
 *  hfn - host frame number
 */
pub const KVM_USER_MEM_SLOTS: u32 = 16;
pub const KVM_PRIVATE_MEM_SLOTS: u32 = 3;
pub const KVM_MEM_SLOTS_NUM: u32 = KVM_USER_MEM_SLOTS + KVM_PRIVATE_MEM_SLOTS;
pub const KVM_ADDRESS_SPACE_NUM: usize = 2;

pub const KVM_MEM_LOG_DIRTY_PAGES: u32 = 1 << 0;
pub const KVM_MEM_READONLY: u32 = 1 << 1;
pub const KVM_MEM_MAX_NR_PAGES: u32 = (1 << 31) - 1;

/*
 * The bit 16 ~ bit 31 of kvm_memory_region::flags are internally used
 * in kvm, other bits are visible for userspace which are defined in
 * include/linux/kvm_h.
 */
pub const KVM_MEMSLOT_INVALID: u32 = 1 << 16;
// pub const  KVM_MEMSLOT_INCOHERENT:u32 = 1 << 17;

// pub const KVM_PERMILLE_MMU_PAGES: u32 = 20; //  the proportion of MMU pages required per thousand (out of 1000) memory pages.
// pub const KVM_MIN_ALLOC_MMU_PAGES: u32 = 64;

pub const PAGE_SHIFT: u32 = 12;
pub const PAGE_SIZE: u32 = 1 << PAGE_SHIFT;
pub const PAGE_MASK: u32 = !(PAGE_SIZE - 1);

/// 通过这个结构可以将虚拟机的物理地址对应到用户进程的虚拟地址
/// 用来表示虚拟机的一段物理内存
#[repr(C)]
#[derive(Default)]
pub struct KvmUserspaceMemoryRegion {
    pub slot: u32, // 要在哪个slot上注册内存区间
    // flags有两个取值，KVM_MEM_LOG_DIRTY_PAGES和KVM_MEM_READONLY，用来指示kvm针对这段内存应该做的事情。
    // KVM_MEM_LOG_DIRTY_PAGES用来开启内存脏页，KVM_MEM_READONLY用来开启内存只读。
    pub flags: u32,
    pub guest_phys_addr: u64, // 虚机内存区间起始物理地址
    pub memory_size: u64,     // 虚机内存区间大小
    pub userspace_addr: u64,  // 虚机内存区间对应的主机虚拟地址
}

#[derive(Default, Clone, Copy, Debug)]
pub struct KvmMemorySlot {
    pub base_gfn: u64,       // 虚机内存区间起始物理页框号
    pub npages: u64,         // 虚机内存区间页数，即内存区间的大小
    pub userspace_addr: u64, // 虚机内存区间对应的主机虚拟地址
    pub flags: u32,          // 虚机内存区间属性
    pub id: u16,             // 虚机内存区间id
                             // 用来记录虚机内存区间的脏页信息，每个bit对应一个页，如果bit为1，表示对应的页是脏页，如果bit为0，表示对应的页是干净页。
                             // pub dirty_bitmap: *mut u8,
                             // unsigned long *rmap[KVM_NR_PAGE_SIZES]; 反向映射相关的结构, 创建EPT页表项时就记录GPA对应的页表项地址(GPA-->页表项地址)，暂时不需要
}

#[derive(Default, Clone, Copy, Debug)]
pub struct KvmMemorySlots {
    pub memslots: [KvmMemorySlot; KVM_MEM_SLOTS_NUM as usize], // 虚机内存区间数组
    pub used_slots: u32,                                       // 已经使用的slot数量
}

#[derive(PartialEq, Eq, Debug)]
pub enum KvmMemoryChange {
    Create,
    Delete,
    Move,
    FlagsOnly,
}

pub fn kvm_vcpu_memslots(_vcpu: &mut dyn Vcpu) -> KvmMemorySlots {
    let kvm = vm(0).unwrap();
    let as_id = 0;
    return kvm.memslots[as_id];
}

fn __gfn_to_memslot(slots: KvmMemorySlots, gfn: u64) -> Option<KvmMemorySlot> {
    kdebug!("__gfn_to_memslot");
    // TODO: 使用二分查找的方式优化
    for i in 0..slots.used_slots {
        let memslot = slots.memslots[i as usize];
        if gfn >= memslot.base_gfn && gfn < memslot.base_gfn + memslot.npages {
            return Some(memslot);
        }
    }
    return None;
}

fn __gfn_to_hva(slot: KvmMemorySlot, gfn: u64) -> u64 {
    return slot.userspace_addr + (gfn - slot.base_gfn) * (PAGE_SIZE as u64);
}
fn __gfn_to_hva_many(
    slot: Option<KvmMemorySlot>,
    gfn: u64,
    nr_pages: Option<&mut u64>,
    write: bool,
) -> Result<u64, SystemError> {
    kdebug!("__gfn_to_hva_many");
    if slot.is_none() {
        return Err(SystemError::KVM_HVA_ERR_BAD);
    }
    let slot = slot.unwrap();
    if slot.flags & KVM_MEMSLOT_INVALID != 0 || (slot.flags & KVM_MEM_READONLY != 0) && write {
        return Err(SystemError::KVM_HVA_ERR_BAD);
    }

    if let Some(nr_pages) = nr_pages {
        *nr_pages = slot.npages - (gfn - slot.base_gfn);
    }

    return Ok(__gfn_to_hva(slot, gfn));
}

/* From Linux kernel
 * Pin guest page in memory and return its pfn.
 * @addr: host virtual address which maps memory to the guest
 * @atomic: whether this function can sleep
 * @async: whether this function need to wait IO complete if the
 *         host page is not in the memory
 * @write_fault: whether we should get a writable host page
 * @writable: whether it allows to map a writable host page for !@write_fault
 *
 * The function will map a writable host page for these two cases:
 * 1): @write_fault = true
 * 2): @write_fault = false && @writable, @writable will tell the caller
 *     whether the mapping is writable.
 */
// 计算 HVA 对应的 pfn，同时确保该物理页在内存中
// host端虚拟地址到物理地址的转换，有两种方式，hva_to_pfn_fast、hva_to_pfn_slow
// 正确性待验证
fn hva_to_pfn(addr: u64, _atomic: bool, _writable: &mut bool) -> Result<u64, SystemError> {
    kdebug!("hva_to_pfn");
    unsafe {
        let raw = addr as *const i32;
        kdebug!("raw={:x}", *raw);
    }
    // let hpa = MMArch::virt_2_phys(VirtAddr::new(addr)).unwrap().data() as u64;
    let hva = VirtAddr::new(addr as usize);
    let mut mapper = KernelMapper::lock();
    let mapper = mapper.as_mut().unwrap();
    if let Some((hpa, _)) = mapper.translate(hva) {
        return Ok(hpa.data() as u64 >> PAGE_SHIFT);
    }
    unsafe {
        mapper.map(hva, PageFlags::mmio_flags());
    }
    let (hpa, _) = mapper.translate(hva).unwrap();
    return Ok(hpa.data() as u64 >> PAGE_SHIFT);
}

pub fn __gfn_to_pfn(
    slot: Option<KvmMemorySlot>,
    gfn: u64,
    atomic: bool,
    write: bool,
    writable: &mut bool,
) -> Result<u64, SystemError> {
    kdebug!("__gfn_to_pfn");
    let mut nr_pages = 0;
    let addr = __gfn_to_hva_many(slot, gfn, Some(&mut nr_pages), write)?;
    let pfn = hva_to_pfn(addr, atomic, writable)?;
    kdebug!("hva={}, pfn={}", addr, pfn);
    return Ok(pfn);
}

pub fn kvm_vcpu_gfn_to_memslot(vcpu: &mut dyn Vcpu, gfn: u64) -> Option<KvmMemorySlot> {
    return __gfn_to_memslot(kvm_vcpu_memslots(vcpu), gfn);
}
