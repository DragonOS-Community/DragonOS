use super::{vcpu::VmxVcpu, kvm_emulation::x86_exception};

pub struct KvmMmuPage{
    gfn: u64,
}

// 暂时还不清楚用来做什么
pub struct KvmMmuPageRole{

}

pub struct KvmMmu{
    set_cr3: fn(&mut VmxVcpu, u64),
    get_cr3: fn(& VmxVcpu) -> u64,
    get_pdptr: fn(& VmxVcpu, index:u32) -> u64, // Page Directory Pointer Table Register?暂时不知道和CR3的区别是什么
    page_fault: fn(&mut VmxVcpu, gva: u64, error_code: u32, prefault:bool) -> u32,
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
