use alloc::boxed::Box;
use bitfield_struct::bitfield;

use crate::{virt::kvm::{KVM, host_mem::{KVM_ADDRESS_SPACE_NUM, KVM_MEM_SLOTS_NUM, KVM_PERMILLE_MMU_PAGES, KVM_MIN_ALLOC_MMU_PAGES, PAGE_SHIFT, kvm_vcpu_gfn_to_memslot, __gfn_to_pfn}, vcpu::Vcpu}, syscall::SystemError, arch::{kvm::vmx::ept::EptMapper, MMArch}, mm::{page::PageFlags, syscall::ProtFlags}};

use super::{vcpu::VmxVcpu, kvm_emulation::x86_exception, vmx_asm_wrapper::{vmx_vmread, vmx_vmwrite}, vmcs::VmcsFields};
use super::super::KVM_NR_MEM_OBJS;
use crate::arch::kvm::vmx::mmu::VmcsFields::CTRL_EPTP_PTR;

pub const PT64_ROOT_LEVEL: u32 = 4;
pub const PT32_ROOT_LEVEL: u32 = 2;
pub const PT32E_ROOT_LEVEL: u32 = 3;

pub struct KvmMmuPage{
    gfn: u64, // 管理地址范围的起始地址对应的 gfn
    role: KvmMmuPageRole, // 基本信息，包括硬件特性和所属层级等
    spt: *mut u64, // spt: shadow page table,指向 struct page 的地址，其包含了所有页表项 (pte)。同时 page->private 会指向该 kvm_mmu_page
}

#[bitfield(u32)]
pub struct KvmMmuPageRole{
    #[bits(4)]
    level: usize, // 页所处的层级
    cr4_pae: bool, // cr4.pae，1 表示使用 64bit gpte
    #[bits(2)]
    quadrant: usize, // 如果 cr4.pae=0，则 gpte 为 32bit，但 spte 为 64bit，因此需要用多个 spte 来表示一个 gpte，该字段指示是 gpte 的第几块
    direct: bool, 
    #[bits(3)]
    access: usize, // 访问权限
    invalid: bool, // 失效，一旦 unpin 就会被销毁
    nxe: bool, // efer.nxe，不可执行
    cr0_wp: bool, // cr0.wp, 写保护
    smep_andnot_wp: bool, // smep && !cr0.wp，SMEP启用，用户模式代码将无法执行位于内核地址空间中的指令。
    smap_andnot_wp: bool, // smap && !cr0.wp
    #[bits(8)]
    unused: usize,
    #[bits(8)]
    smm: usize, // 1 表示处于 system management mode, 0 表示非 SMM
}

//  We don't want allocation failures within the mmu code, so we preallocate
// enough memory for a single page fault in a cache.
pub struct KvmMmuMemoryCache {
    num_objs: u32,
    objs: [*mut u8; KVM_NR_MEM_OBJS as usize],
}

#[derive(Default)]
pub struct KvmMmu{
    set_cr3: fn(&mut VmxVcpu, u64),
    get_cr3: fn(& VmxVcpu) -> u64,
    get_pdptr: fn(& VmxVcpu, index:u32) -> u64, // Page Directory Pointer Table Register?暂时不知道和CR3的区别是什么
    page_fault: fn(&mut VmxVcpu, gpa: u64, error_code: u32, prefault:bool) ->  Result<u32, SystemError>,
    inject_page_fault: fn(&mut VmxVcpu, fault: &x86_exception),
    gva_to_gpa: fn(&mut VmxVcpu, gva: u64, access: u32, exception: &x86_exception) -> u64,
    translate_gpa: fn(&mut VmxVcpu, gpa: u64, access: u32, exception: &x86_exception) -> u64,
    sync_page: fn(&mut VmxVcpu, &mut KvmMmuPage),
    invlpg: fn(&mut VmxVcpu, gva: u64), // invalid entry
    update_pte: fn(&mut VmxVcpu, sp: &KvmMmuPage, spte: u64, pte: u64),

    root_hpa: u64,
    root_level: u32,
    // shadow_root_level: u32, // 暂时不需要, shadow page table的实现需要
    base_role: KvmMmuPageRole,
    direct_map: bool,
    // ...还有一些变量不知道用来做什么
}

/*
 * Caculate mmu pages needed for kvm.
 */
pub fn kvm_mmu_calculate_mmu_pages() -> u32 {
	let nr_mmu_pages:u32;
    let nr_pages = 0;
       
    let kvm = KVM();
    for as_id in 0..KVM_ADDRESS_SPACE_NUM {
        let slots = kvm.memslots[as_id];
        for i in 0..KVM_MEM_SLOTS_NUM {
            let memslot = slots.memslots[i as usize];
            nr_pages += memslot.npages;
        }
    }
	
	nr_mmu_pages = nr_pages * KVM_PERMILLE_MMU_PAGES / 1000;
	nr_mmu_pages = nr_mmu_pages.max(KVM_MIN_ALLOC_MMU_PAGES);
	return nr_mmu_pages;
}

pub fn kvm_mmu_change_mmu_pages(goal_nr_mmu_pages: u32){
    let kvm = KVM();
    // 释放多余的mmu page
    if kvm.arch.n_used_mmu_pages > goal_nr_mmu_pages {
        while kvm.arch.n_used_mmu_pages > goal_nr_mmu_pages {
            if !prepare_zap_oldest_mmu_page() {
                break;
            }
        }
        // kvm_mmu_commit_zap_page();
        goal_nr_mmu_pages = kvm.arch.n_used_mmu_pages;

    }
    kvm.arch.n_max_mmu_pages = goal_nr_mmu_pages;
}

pub fn prepare_zap_oldest_mmu_page() {

}

pub fn kvm_mmu_setup(vcpu: &mut dyn Vcpu) {
    // TODO: init_kvm_softmmu(vcpu), init_kvm_nested_mmu(vcpu)
    init_kvm_tdp_mmu(vcpu);
}

pub fn init_kvm_tdp_mmu(vcpu: &mut dyn Vcpu){
    let context = &mut vcpu.mmu;
    context.page_fault = tdp_page_fault;
    context.get_cr3 = get_tdp_cr3;
    context.set_eptp = set_tdp_eptp;
	// context.inject_page_fault = kvm_inject_page_fault; TODO: inject_page_fault
	// context.invlpg = nonpaging_invlpg;
	// context.sync_page = nonpaging_sync_page;
	// context.update_pte = nonpaging_update_pte;

    // TODO: gva to gpa in kvm
    // if !is_paging(vcpu) { // vcpu不分页
    //     context.gva_to_gpa = nonpaging_gva_to_gpa;
	// 	context.root_level = 0;
    // } else if (is_long_mode(vcpu)) {
	// 	context.gva_to_gpa = paging64_gva_to_gpa;
	// 	context.root_level = PT64_ROOT_LEVEL;
    // TODO:: different paging strategy
    // } else if (is_pae(vcpu)) {
    //     context.gva_to_gpa = paging64_gva_to_gpa;
    //     context.root_level = PT32E_ROOT_LEVEL;
    // } else {
    //     context.gva_to_gpa = paging32_gva_to_gpa;
    //     context.root_level = PT32_ROOT_LEVEL;
    // }
}

pub fn tdp_page_fault(vcpu: &mut VmxVcpu, gpa: u64, error_code: u32, prefault: bool) -> Result<u32, SystemError> {
    let gfn = gpa >> PAGE_SHIFT; // 物理地址右移12位得到物理页框号(相对于虚拟机而言)
    // 分配缓存池，为了避免在运行时分配空间失败，这里提前分配/填充足额的空间
    mmu_topup_memory_caches(vcpu)?; 
    // TODO：获取gfn使用的level，处理hugepage的问题
    let level = 1; // 4KB page
    // TODO: 快速处理由读写操作引起violation，即present同时有写权限的非mmio page fault
    // fast_page_fault(vcpu, gpa, level, error_code)
    // gfn->pfn
    let mut map_writable = false;
    let write = error_code & ((1 as u32) << 1);
    let pfn = mmu_gfn_to_pfn_fast(vcpu, gpa, prefault, gfn, write, &mut map_writable)?;
    // direct map就是映射ept页表的过程
    __direct_map(vcpu, gpa,write, map_writable,
        level, gfn, pfn, prefault)?;
    Ok(())
}

pub fn get_tdp_cr3(vcpu: &VmxVcpu) -> u64 {
    let guest_cr3 = vmx_vmread(VmcsFields::GUEST_CR3 as u32)
        .expect("Failed to read eptp");
    return guest_cr3;
}

pub fn set_tdp_eptp(mut root_hpa: u64) {
    // 设置权限位，目前是写死的，可读可写可执行
    let mut eptp = 0x7 as u64;
	eptp |= root_hpa & MMArch::PAGE_MASK;
    vmx_vmwrite(CTRL_EPTP_PTR, eptp);
}

pub fn __direct_map(vcpu: &mut VmxVcpu, gpa: u64, write: u32,
    map_writable: bool, level: i32, gfn: u64, pfn: u64,
    prefault: bool) -> Result<u32, SystemError> {
        // 判断vcpu.mmu.root_hpa是否有效
        if vcpu.mmu.root_hpa == 0 {
            return Err(SystemError::KVM_HVA_ERR_BAD);
        }
        // 把gpa映射到hpa
        let mut ept_mapper = EptMapper::lock();
        let page_flags = PageFlags::from_prot_flags(
            ProtFlags::from_bits_truncate(0x7 as u64),
            false
        );
        unsafe {
            assert!(ept_mapper
                .walk(gpa, pfn << PAGE_SHIFT,  page_flags)
                .is_ok());
        }
        drop(ept_mapper);
}

pub fn mmu_gfn_to_pfn_fast(
    vcpu: &mut VmxVcpu, 
    gpa: u64, 
    prefault: bool, 
    gfn: u64, 
    write: bool,
    writable: &mut bool) -> Result<u64, SystemError> {
    let slot = kvm_vcpu_gfn_to_memslot(vcpu, gfn)?;
    let pfn = __gfn_to_pfn(slot, gfn, false,  write, writable)?;
    Ok(pfn)
}

// TODO: 添加cache
pub fn mmu_topup_memory_caches(vcpu: &mut VmxVcpu)->Result<u32, SystemError>{
    // 如果 vcpu->arch.mmu_page_header_cache 不足，从 mmu_page_header_cache 中分配
    // pte_list_desc_cache 和 mmu_page_header_cache 两块全局 slab cache 在 kvm_mmu_module_init 中被创建
    // mmu_topup_memory_cache(vcpu.mmu_page_header_cache,
    //     mmu_page_header_cache, 4);
    Ok(())
}