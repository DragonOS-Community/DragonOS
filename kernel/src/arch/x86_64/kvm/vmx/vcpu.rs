use x86::{
    controlregs, msr, segmentation
};
use raw_cpuid::CpuId;
use x86;
use crate::arch::kvm::vmx::mmu::KvmMmu;
use crate::arch::mm::{LockedFrameAllocator, PageMapper};
use crate::virt::kvm::GUEST_STACK_SIZE;
use crate::{kdebug, printk_color, GREEN, BLACK};
use alloc::boxed::Box;
use alloc::alloc::Global;
use alloc::sync::Arc;
use core::arch::asm;
use core::slice;
use crate::libs::mutex::Mutex;
use crate::arch::MMArch;
use crate::mm::{MemoryManagementArch, PageTableKind};
use crate::mm::{VirtAddr, phys_2_virt};
use crate::syscall::SystemError;
use crate::virt::kvm::hypervisor::Hypervisor;
use crate::virt::kvm::vcpu::Vcpu;
use crate::arch::x86_64::mm::X86_64MMArch;
use super::mmu::KvmMmuEpt;
use super::vmcs::{VMCSRegion, VmcsFields, 
    VmxEntryCtrl, VmxPrimaryExitCtrl,
    VmxPrimaryProcessBasedExecuteCtrl, VmxSecondaryProcessBasedExecuteCtrl
};
use super::vmx_asm_wrapper::{
    vmxon, vmxoff, vmx_vmwrite, vmx_vmread, vmx_vmlaunch, vmx_vmptrld, vmx_vmclear
};
// use crate::virt::kvm::{KVM};

// KERNEL_ALLOCATOR
pub const PAGE_SIZE: usize = 0x1000;
pub const NR_VCPU_REGS: usize = 16;

#[repr(C, align(4096))]
pub struct VmxonRegion {
    pub revision_id: u32,
    pub data: [u8; PAGE_SIZE - 4],
}

#[repr(C, align(4096))]
pub struct MSRBitmap {
    pub data: [u8; PAGE_SIZE],
}

pub struct VcpuData {
    /// The virtual and physical address of the Vmxon naturally aligned 4-KByte region of memory
    pub vmxon_region: Box<VmxonRegion>,
    pub vmxon_region_physical_address: u64,  // vmxon需要该地址
    /// The virtual and physical address of the Vmcs naturally aligned 4-KByte region of memory
    /// holds the complete CPU state of both the host and the guest.
    /// includes the segment registers, GDT, IDT, TR, various MSR’s
    /// and control field structures for handling exit and entry operations
    pub vmcs_region: Box<VMCSRegion>,
    pub vmcs_region_physical_address: u64,  // vmptrld, vmclear需要该地址
    pub msr_bitmap: Box<MSRBitmap>,
    pub msr_bitmap_physical_address: u64,
}

pub struct VcpuContextFrame{
    regs: [usize; NR_VCPU_REGS], // 通用寄存器
}


pub enum VcpuState {
    VcpuInv = 0,
    VcpuPend = 1,
    VcpuAct = 2,
}

pub struct VmxVcpu {
    pub vcpu_id: u32,
    pub vcpu_ctx: VcpuContextFrame, // 保存vcpu切换时的上下文，如通用寄存器等
    pub vcpu_state: VcpuState, // vcpu当前运行状态
    pub mmu: KvmMmuEpt,    // vcpu的内存管理单元
    pub data: VcpuData, // vcpu的数据
    pub hypervisor: Arc<Mutex<Hypervisor>>, // parent KVM
}

impl VcpuData {
    pub fn alloc() -> Result<Self, SystemError> {
        let vmxon_region = unsafe { Box::try_new_zeroed_in(Global).expect("Try new zeroed fail!").assume_init() };
        let vmcs_region = unsafe { Box::try_new_zeroed_in(Global).expect("Try new zeroed fail!").assume_init() };
        let msr_bitmap = unsafe { Box::try_new_zeroed_in(Global).expect("Try new zeroed fail!").assume_init() };
        let mut instance = Self {
            // Allocate a naturally aligned 4-KByte VMXON region of memory to enable VMX operation (Intel Manual: 25.11.5 VMXON Region)
            vmxon_region,
            // FIXME: virt_2_phys的转换正确性存疑
            vmxon_region_physical_address: {
                let vaddr = VirtAddr::new(vmxon_region.as_ref() as *const _ as _);
                unsafe {MMArch::virt_2_phys(vaddr).unwrap().data() as u64}
            },
            // Allocate a naturally aligned 4-KByte VMCS region of memory 
            vmcs_region,
            vmcs_region_physical_address: {
                let vaddr = VirtAddr::new(vmcs_region.as_ref() as *const _ as _);
                unsafe {MMArch::virt_2_phys(vaddr).unwrap().data() as u64}
            },
            msr_bitmap,
            msr_bitmap_physical_address: {
                let vaddr = VirtAddr::new(msr_bitmap.as_ref() as *const _ as _);
                unsafe {MMArch::virt_2_phys(vaddr).unwrap().data() as u64}
            },
        };
        printk_color!(GREEN, BLACK, "[+] init_region\n");
        instance.init_region()?;
        Ok(instance)
    }

    pub fn init_region(&mut self) -> Result<(), SystemError> {
        // Get the Virtual Machine Control Structure revision identifier (VMCS revision ID) 
        // (Intel Manual: 25.11.5 VMXON Region)
        let revision_id = unsafe { (msr::rdmsr(msr::IA32_VMX_BASIC) as u32) & 0x7FFF_FFFF };
        kdebug!("[+] VMXON Region Virtual Address: {:p}", self.vmxon_region);
        kdebug!("[+] VMXON Region Physical Addresss: 0x{:x}", self.vmxon_region_physical_address);
        kdebug!("[+] VMCS Region Virtual Address: {:p}", self.vmcs_region);
        kdebug!("[+] VMCS Region Physical Address1: 0x{:x}", self.vmcs_region_physical_address);
        self.vmxon_region.revision_id = revision_id;
        self.vmcs_region.revision_id = revision_id;
        return Ok(());
    }

}


impl VmxVcpu {
    pub fn new(vcpu_id: u32, hypervisor: Arc<Mutex<Hypervisor>>) -> Result<Self, SystemError> {
        kdebug!("Creating processor {}", vcpu_id);
        let instance = Self {
            vcpu_id,
            vcpu_ctx: VcpuContextFrame{
                regs: [0; NR_VCPU_REGS],
            },
            vcpu_state: VcpuState::VcpuInv,
            mmu: KvmMmuEpt::default(),
            data: VcpuData::alloc()?, 
            hypervisor: hypervisor.clone(),
        };
        Ok(instance)
    }

    pub fn vmcs_init_guest(&self) -> Result<(), SystemError>{
        // segment field initialization
        vmx_vmwrite(VmcsFields::GUEST_ES_SELECTOR as u32, (segmentation::es().bits() ).into())?;
        vmx_vmwrite(VmcsFields::GUEST_CS_SELECTOR as u32, (segmentation::cs().bits() ).into())?;
        vmx_vmwrite(VmcsFields::GUEST_SS_SELECTOR as u32, (segmentation::ss().bits() ).into())?;
        vmx_vmwrite(VmcsFields::GUEST_DS_SELECTOR as u32, (segmentation::ds().bits() ).into())?;
        vmx_vmwrite(VmcsFields::GUEST_FS_SELECTOR as u32, (segmentation::fs().bits() & (!0x07)).into())?;
        vmx_vmwrite(VmcsFields::GUEST_GS_SELECTOR as u32, (segmentation::gs().bits() & (!0x07)).into())?;

        vmx_vmwrite(VmcsFields::GUEST_LDTR_SELECTOR as u32, 0)?;
        vmx_vmwrite(VmcsFields::GUEST_TR_SELECTOR as u32, unsafe{ x86::task::tr().bits().into() })?;
        vmx_vmwrite(VmcsFields::GUEST_INTR_STATUS as u32, 0)?;
        vmx_vmwrite(VmcsFields::GUEST_PML_INDEX as u32, 0)?;

        vmx_vmwrite(VmcsFields::GUEST_VMCS_LINK_PTR as u32, u64::MAX)?;
        vmx_vmwrite(VmcsFields::GUEST_DEBUGCTL as u32, 0)?;

        vmx_vmwrite(VmcsFields::GUEST_CS_LIMIT as u32,  u64::MAX)?;
        vmx_vmwrite(VmcsFields::GUEST_SS_LIMIT as u32,  u64::MAX)?;
        vmx_vmwrite(VmcsFields::GUEST_DS_LIMIT as u32,  u64::MAX)?;
        vmx_vmwrite(VmcsFields::GUEST_ES_LIMIT as u32,  u64::MAX)?;
        vmx_vmwrite(VmcsFields::GUEST_FS_LIMIT as u32,  u64::MAX)?;
        vmx_vmwrite(VmcsFields::GUEST_GS_LIMIT as u32,  u64::MAX)?;
        vmx_vmwrite(VmcsFields::GUEST_TR_LIMIT as u32, 0x67 as u64)?;
        vmx_vmwrite(VmcsFields::GUEST_LDTR_LIMIT as u32, u64::MAX )?;
        vmx_vmwrite(
            VmcsFields::GUEST_GDTR_LIMIT as u32, 
            unsafe{
                let mut pseudo_descriptpr: x86::dtables::DescriptorTablePointer<u64> = Default::default();
                x86::dtables::sgdt(&mut pseudo_descriptpr);
                pseudo_descriptpr.limit.into()
            }
        )?;
        vmx_vmwrite(
            VmcsFields::GUEST_IDTR_LIMIT as u32, 
            unsafe{
                let mut pseudo_descriptpr: x86::dtables::DescriptorTablePointer<u64> = Default::default();
                x86::dtables::sidt(&mut pseudo_descriptpr);
                pseudo_descriptpr.limit.into()
            }
        )?;

        vmx_vmwrite(VmcsFields::GUEST_ES_ACCESS_RIGHTS as u32, 
            if vmx_vmread(VmcsFields::GUEST_ES_SELECTOR as u32).unwrap() == 0 {0x10000}  else{ 0xc093} )?;
        vmx_vmwrite(VmcsFields::GUEST_CS_ACCESS_RIGHTS as u32, 0xa09b)?;
        vmx_vmwrite(VmcsFields::GUEST_SS_ACCESS_RIGHTS as u32, 0xc093)?;
        vmx_vmwrite(VmcsFields::GUEST_DS_ACCESS_RIGHTS as u32,
            if vmx_vmread(VmcsFields::GUEST_DS_SELECTOR as u32).unwrap() == 0 {0x10000}  else{0xc093})?;
        vmx_vmwrite(VmcsFields::GUEST_FS_ACCESS_RIGHTS as u32,
            if vmx_vmread(VmcsFields::GUEST_FS_SELECTOR as u32).unwrap() == 0 {0x10000}  else{0xc093})?;
        vmx_vmwrite(VmcsFields::GUEST_GS_ACCESS_RIGHTS as u32,
            if vmx_vmread(VmcsFields::GUEST_GS_SELECTOR as u32).unwrap() == 0 {0x10000}  else{0xc093})?;
        vmx_vmwrite(VmcsFields::GUEST_LDTR_ACCESS_RIGHTS as u32, 0x10000)?;
        vmx_vmwrite(VmcsFields::GUEST_TR_ACCESS_RIGHTS as u32, 0x8b)?;
        vmx_vmwrite(VmcsFields::GUEST_INTERRUPTIBILITY_STATE as u32, 0)?;
        vmx_vmwrite(VmcsFields::GUEST_ACTIVITY_STATE as u32, 0)?;
        vmx_vmwrite(VmcsFields::GUEST_SYSENTER_CS as u32, vmx_vmread(VmcsFields::HOST_SYSENTER_CS as u32).unwrap())?;
        vmx_vmwrite(VmcsFields::GUEST_VMX_PREEMPT_TIMER_VALUE as u32, 0)?;

        vmx_vmwrite(VmcsFields::GUEST_ES_BASE as u32, 0)?;
        vmx_vmwrite(VmcsFields::GUEST_CS_BASE as u32, 0)?;
        vmx_vmwrite(VmcsFields::GUEST_SS_BASE as u32, 0)?;
        vmx_vmwrite(VmcsFields::GUEST_DS_BASE as u32, 0)?;
        vmx_vmwrite(VmcsFields::GUEST_FS_BASE as u32, unsafe {msr::rdmsr(msr::IA32_FS_BASE as u32)})?;
        vmx_vmwrite(VmcsFields::GUEST_GS_BASE as u32, unsafe {msr::rdmsr(msr::IA32_GS_BASE as u32)})?;
        vmx_vmwrite(VmcsFields::GUEST_LDTR_BASE as u32, 0)?;
        vmx_vmwrite(VmcsFields::GUEST_TR_BASE as u32, vmx_vmread(VmcsFields::HOST_TR_BASE as u32).unwrap())?;
        vmx_vmwrite(VmcsFields::GUEST_GDTR_BASE as u32, vmx_vmread(VmcsFields::HOST_GDTR_BASE as u32).unwrap())?;
        vmx_vmwrite(VmcsFields::GUEST_IDTR_BASE as u32, vmx_vmread(VmcsFields::HOST_IDTR_BASE as u32).unwrap())?;
        vmx_vmwrite(VmcsFields::GUEST_RFLAGS as u32, 2)?;
        vmx_vmwrite(VmcsFields::GUEST_SYSENTER_ESP as u32, vmx_vmread(VmcsFields::HOST_SYSENTER_ESP as u32).unwrap())?;
        vmx_vmwrite(VmcsFields::GUEST_SYSENTER_EIP as u32, vmx_vmread(VmcsFields::HOST_SYSENTER_EIP as u32).unwrap())?;

        vmx_vmwrite(
            VmcsFields::GUEST_CR0 as u32, 
            unsafe{ controlregs::cr0().bits().try_into().unwrap() }
        )?;
        vmx_vmwrite(
            VmcsFields::GUEST_CR3 as u32, 
            unsafe{ controlregs::cr3() }
        )?;
        vmx_vmwrite(
            VmcsFields::GUEST_CR4 as u32, 
            unsafe{ controlregs::cr4().bits().try_into().unwrap() }
        )?;
        vmx_vmwrite(
            VmcsFields::GUEST_DR7 as u32, 
            unsafe{ x86::debugregs::dr7().0 as u64 }
        )?;
        vmx_vmwrite(VmcsFields::GUEST_RSP as u32, self.guest_stack.as_ptr() as u64 + GUEST_STACK_SIZE as u64)?;
        vmx_vmwrite(VmcsFields::GUEST_RIP as u32, self.data.guest_rip as u64)?;
        kdebug!("vmcs init guest rip: {:#x}", self.data.guest_rip as u64);
        kdebug!("vmcs init guest rsp: {:#x}", self.guest_stack.as_ptr() as u64 + GUEST_STACK_SIZE as u64);

        // vmx_vmwrite(VmcsFields::GUEST_RFLAGS as u32, x86::bits64::rflags::read().bits())?;
        vmx_vmwrite(VmcsFields::GUEST_DEBUGCTL as u32, unsafe {msr::rdmsr(msr::IA32_DEBUGCTL)})?;


        Ok(())
    }

    pub fn vmcs_init_host(&self) -> Result<(), SystemError>{
        vmx_vmwrite(
            VmcsFields::HOST_CR0 as u32, 
            unsafe{ controlregs::cr0().bits().try_into().unwrap() }
        )?;
        vmx_vmwrite(
            VmcsFields::HOST_CR3 as u32, 
            unsafe{ controlregs::cr3() }
        )?;
        vmx_vmwrite(
            VmcsFields::HOST_CR4 as u32, 
            unsafe{ controlregs::cr4().bits().try_into().unwrap() }
        )?;
        vmx_vmwrite(VmcsFields::HOST_ES_SELECTOR as u32, (segmentation::es().bits() & (!0x07) ).into())?;
        vmx_vmwrite(VmcsFields::HOST_CS_SELECTOR as u32, (segmentation::cs().bits() & (!0x07)).into())?;
        vmx_vmwrite(VmcsFields::HOST_SS_SELECTOR as u32, (segmentation::ss().bits() & (!0x07)).into())?;
        vmx_vmwrite(VmcsFields::HOST_DS_SELECTOR as u32, (segmentation::ds().bits() & (!0x07)).into())?;
        vmx_vmwrite(VmcsFields::HOST_FS_SELECTOR as u32, (segmentation::fs().bits() & (!0x07)).into())?;
        vmx_vmwrite(VmcsFields::HOST_GS_SELECTOR as u32, (segmentation::gs().bits() & (!0x07)).into())?;
        vmx_vmwrite(VmcsFields::HOST_TR_SELECTOR as u32, unsafe{ (x86::task::tr().bits() & (!0x07)).into() })?;
        vmx_vmwrite(VmcsFields::HOST_FS_BASE as u32, unsafe {msr::rdmsr(msr::IA32_FS_BASE)})?;
        vmx_vmwrite(VmcsFields::HOST_GS_BASE as u32, unsafe {msr::rdmsr(msr::IA32_GS_BASE)})?;

        

        let mut pseudo_descriptpr: x86::dtables::DescriptorTablePointer<u64> = Default::default();
        unsafe {x86::dtables::sgdt(&mut pseudo_descriptpr);};

        vmx_vmwrite(
            VmcsFields::HOST_TR_BASE as u32, 
            get_segment_base(
                pseudo_descriptpr.base,
                pseudo_descriptpr.limit,
                unsafe{ x86::task::tr().bits().into() }
            )
        )?;
        vmx_vmwrite(
            VmcsFields::HOST_GDTR_BASE as u32, 
            pseudo_descriptpr.base.to_bits() as u64
        )?;
        vmx_vmwrite(
            VmcsFields::HOST_IDTR_BASE as u32, 
            unsafe{
                let mut pseudo_descriptpr: x86::dtables::DescriptorTablePointer<u64> = Default::default();
                x86::dtables::sidt(&mut pseudo_descriptpr);
                pseudo_descriptpr.base.to_bits() as u64
            }
        )?;

        // fast entry into the kernel
        vmx_vmwrite(VmcsFields::HOST_SYSENTER_ESP as u32, unsafe {msr::rdmsr(msr::IA32_SYSENTER_ESP)})?;
        vmx_vmwrite(VmcsFields::HOST_SYSENTER_EIP as u32, unsafe {msr::rdmsr(msr::IA32_SYSENTER_EIP)})?;
        vmx_vmwrite(VmcsFields::HOST_SYSENTER_CS as u32, unsafe {msr::rdmsr(msr::IA32_SYSENTER_CS)})?;

        // vmx_vmwrite(VmcsFields::HOST_RIP as u32, vmx_return as *const () as u64)?;
        // kdebug!("vmcs init host rip: {:#x}", vmx_return as *const () as u64);
        
        Ok(())
    }
    // Intel SDM Volume 3C Chapter 25.3 “Organization of VMCS Data”
    pub fn vmcs_init(&self) -> Result<(), SystemError> {
        vmx_vmwrite(VmcsFields::CTRL_PIN_BASED_VM_EXEC_CTRLS as u32, adjust_vmx_pinbased_controls() as u64)?;

        vmx_vmwrite(VmcsFields::CTRL_MSR_BITMAP_ADDR as u32, self.data.msr_bitmap_physical_address)?;
        vmx_vmwrite(
            VmcsFields::CTRL_CR0_READ_SHADOW as u32, 
            unsafe{ controlregs::cr0().bits().try_into().unwrap() }
        )?;
        vmx_vmwrite(
            VmcsFields::CTRL_CR4_READ_SHADOW as u32, 
            unsafe{ controlregs::cr4().bits().try_into().unwrap() }
        )?;
        vmx_vmwrite(VmcsFields::CTRL_VM_ENTRY_CTRLS as u32, adjust_vmx_entry_controls() as u64)?;
        vmx_vmwrite(VmcsFields::CTRL_PRIMARY_VM_EXIT_CTRLS as u32, adjust_vmx_exit_controls() as u64)?;
        vmx_vmwrite(VmcsFields::CTRL_PRIMARY_PROCESSOR_VM_EXEC_CTRLS as u32, adjust_vmx_primary_process_exec_controls() as u64)?;
        vmx_vmwrite(VmcsFields::CTRL_SECONDARY_PROCESSOR_VM_EXEC_CTRLS as u32, adjust_vmx_secondary_process_exec_controls() as u64)?;

        self.vmcs_init_host()?;
        self.vmcs_init_guest()?;
        Ok(())
    }

    fn kvm_mmu_load(&self) -> Result<(), SystemError> {
        kdebug!("kvm_mmu_load!");
        // 申请并创建新的页表
        let mapper: crate::mm::page::PageMapper<X86_64MMArch, LockedFrameAllocator> = unsafe {
            PageMapper::create(PageTableKind::EPT, LockedFrameAllocator)
                .ok_or(SystemError::ENOMEM)?
        };

        let ept_root_hpa = mapper.table().phys();
        self.mmu.set_eptp(ept_root_hpa.data() as u64);
        return Ok(());
    }
}

impl Vcpu for VmxVcpu {
    /// Virtualize the CPU
    fn virtualize_cpu(&self) -> Result<(), SystemError> {
        match has_intel_vmx_support() {
            Ok(_) => { kdebug!("[+] CPU supports Intel VMX"); },
            Err(e) => {
                kdebug!("[-] CPU does not support Intel VMX: {:?}", e);
                return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
            }
        };
        
        match enable_vmx_operation(){
            Ok(_) => { kdebug!("[+] Enabling Virtual Machine Extensions (VMX)"); },
            Err(_) => {
                kdebug!("[-] VMX operation is not supported on this processor.");
                return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
            }
        }

        vmxon(self.data.vmxon_region_physical_address)?;
        kdebug!("[+] VMXON successful!");
        vmx_vmclear(self.data.vmcs_region_physical_address)?;
        vmx_vmptrld(self.data.vmcs_region_physical_address)?;
        kdebug!("[+] VMPTRLD successful!");
        self.vmcs_init().expect("vncs_init fail");
        kdebug!("[+] VMCS init!");
        // kdebug!("vmcs init host rip: {:#x}", vmx_return as *const () as u64);
        // kdebug!("vmcs init host rsp: {:#x}", x86::bits64::registers::rsp());
        // vmx_vmwrite(VmcsFields::HOST_RSP as u32, x86::bits64::registers::rsp())?;
        // vmx_vmwrite(VmcsFields::HOST_RIP as u32, vmx_return as *const () as u64)?;
        // vmx_vmwrite(VmcsFields::HOST_RSP as u32,  x86::bits64::registers::rsp())?;
        self.kvm_mmu_load();

        match vmx_vmlaunch() {
            Ok(_) => {},
            Err(e) => {
                let vmx_err = vmx_vmread(VmcsFields::VMEXIT_INSTR_ERR as u32).unwrap();
                kdebug!("vmlaunch failed: {:?}", vmx_err);
                return Err(e);
            },
        }
        Ok(())
    }

    fn devirtualize_cpu(&self) -> Result<(), SystemError> {
        vmxoff()?;
        Ok(())
    }

    /// Gets the index of the current logical/virtual processor
    fn id(&self) -> u32 {
        self.vcpu_id
    }
}


pub fn get_segment_base(gdt_base: *const u64, gdt_size: u16, segment_selector: u16) -> u64{
	let table = segment_selector & 0x0004; // get table indicator in selector
    let index = (segment_selector >> 3) as usize; // get index in selector
    if table==0 && index==0 {
        return 0;
    }
    let descriptor_table = unsafe {slice::from_raw_parts(gdt_base, gdt_size.into())};
    let descriptor = descriptor_table[index] ;
    
    let base_high = (descriptor & 0xFF00_0000_0000_0000) >> 32;
    let base_mid = (descriptor & 0x0000_00FF_0000_0000) >> 16;
    let base_low = (descriptor & 0x0000_0000_FFFF_0000) >> 16;
    let segment_base = (base_high | base_mid | base_low) & 0xFFFFFFFF;
    let virtaddr = phys_2_virt(segment_base.try_into().unwrap()).try_into().unwrap();
    kdebug!("segment_base={:x}", phys_2_virt(segment_base.try_into().unwrap()));
    return virtaddr;
}

// FIXME: may have bug
pub fn read_segment_access_rights(segement_selector: u16) -> u32{
    let table = segement_selector & 0x0004; // get table indicator in selector
    let index = segement_selector & 0xFFF8; // get index in selector
    let mut flag: u16;
    if table==0 && index==0 {
        return 0;
    }
    unsafe{
        asm!(
            "lar {0:r}, rcx",
            "mov {1:r}, {0:r}",
            in(reg) segement_selector,
            out(reg) flag,
        );
    }
    return (flag >> 8) as u32;
}
pub fn adjust_vmx_controls(ctl_min:u32, ctl_opt:u32, msr:u32, result: &mut u32){
    let vmx_msr_low:u32 = unsafe {(msr::rdmsr(msr) & 0x0000_0000_FFFF_FFFF) as u32};
    let vmx_msr_high:u32 = unsafe {(msr::rdmsr(msr) >> 32) as u32};
    let mut ctl: u32 = ctl_min | ctl_opt;
    ctl &= vmx_msr_high; /* bit == 0 in high word ==> must be zero */
    ctl |= vmx_msr_low;  /* bit == 1 in low word  ==> must be one  */
    *result = ctl;
}

pub fn adjust_vmx_entry_controls() -> u32 {
    let mut entry_controls:u32 = 0;
    adjust_vmx_controls(
        VmxEntryCtrl::LOAD_DBG_CTRLS.bits(),
        VmxEntryCtrl::IA32E_MODE_GUEST.bits(),
        msr::IA32_VMX_ENTRY_CTLS,  //Capability Reporting Register of VM-entry Controls (R/O) 
        &mut entry_controls
    );
    return entry_controls;
    // msr::IA32_VMX_TRUE_ENTRY_CTLS//Capability Reporting Register of VM-entry Flex Controls (R/O) See Table 35-2
}

pub fn adjust_vmx_exit_controls() -> u32 {
    let mut exit_controls:u32 = 0;
    adjust_vmx_controls(
        VmxPrimaryExitCtrl::SAVE_DBG_CTRLS.bits(),
        VmxPrimaryExitCtrl::HOST_ADDR_SPACE_SIZE.bits(),
        msr::IA32_VMX_EXIT_CTLS,  
        &mut exit_controls
    );
    return exit_controls;
}

pub fn adjust_vmx_pinbased_controls() -> u32 {
    let mut controls:u32 = 0;
    adjust_vmx_controls(0,0,msr::IA32_VMX_TRUE_PINBASED_CTLS, &mut controls);
    return controls;
}

pub fn adjust_vmx_primary_process_exec_controls() -> u32 {
    let mut controls:u32 = 0;
    adjust_vmx_controls(
        0,
        VmxPrimaryProcessBasedExecuteCtrl::USE_MSR_BITMAPS.bits() | 
        VmxPrimaryProcessBasedExecuteCtrl::ACTIVATE_SECONDARY_CONTROLS.bits(),
        msr::IA32_VMX_PROCBASED_CTLS,  
        &mut controls
    );
    return controls;
}

pub fn adjust_vmx_secondary_process_exec_controls() -> u32 {
    let mut controls:u32 = 0;
    adjust_vmx_controls(
        0,
        VmxSecondaryProcessBasedExecuteCtrl::ENABLE_RDTSCP.bits() | 
        VmxSecondaryProcessBasedExecuteCtrl::ENABLE_XSAVES_XRSTORS.bits() |
        VmxSecondaryProcessBasedExecuteCtrl::ENABLE_INVPCID.bits() |
        VmxSecondaryProcessBasedExecuteCtrl::ENABLE_EPT.bits(),
        msr::IA32_VMX_PROCBASED_CTLS2,  
        &mut controls
    );
    return controls;
}



/// Check to see if CPU is Intel (“GenuineIntel”).
/// Check processor supports for Virtual Machine Extension (VMX) technology 
//  CPUID.1:ECX.VMX[bit 5] = 1 (Intel Manual: 24.6 Discovering Support for VMX)
pub fn has_intel_vmx_support() -> Result<(), SystemError> {
    let cpuid = CpuId::new();
    if let Some(vi) = cpuid.get_vendor_info() {
        if vi.as_str() != "GenuineIntel" {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
    }
    if let Some(fi) = cpuid.get_feature_info(){
        if !fi.has_vmx() {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
    }
    Ok(())
}

/// Enables Virtual Machine Extensions 
// - CR4.VMXE[bit 13] = 1 (Intel Manual: 24.7 Enabling and Entering VMX Operation)
pub fn enable_vmx_operation() -> Result<(), SystemError> {
    let mut cr4 = unsafe { controlregs::cr4() };
    cr4.set(controlregs::Cr4::CR4_ENABLE_VMX, true);
    unsafe { controlregs::cr4_write(cr4) };

    set_lock_bit()?;
    kdebug!("[+] Lock bit set via IA32_FEATURE_CONTROL");
    set_cr0_bits();
    kdebug!("[+] Mandatory bits in CR0 set/cleared");
    set_cr4_bits();
    kdebug!("[+] Mandatory bits in CR4 set/cleared");

    Ok(())
}

/// Check if we need to set bits in IA32_FEATURE_CONTROL 
// (Intel Manual: 24.7 Enabling and Entering VMX Operation)
fn set_lock_bit() -> Result<(), SystemError> {
    const VMX_LOCK_BIT: u64 = 1 << 0;
    const VMXON_OUTSIDE_SMX: u64 = 1 << 2;

    let ia32_feature_control = unsafe { msr::rdmsr(msr::IA32_FEATURE_CONTROL) };

    if (ia32_feature_control & VMX_LOCK_BIT) == 0 {
        unsafe {
            msr::wrmsr(
                msr::IA32_FEATURE_CONTROL,
                VMXON_OUTSIDE_SMX | VMX_LOCK_BIT | ia32_feature_control,
            )
        };
    } else if (ia32_feature_control & VMXON_OUTSIDE_SMX) == 0 {
        return Err(SystemError::EPERM);
    }

    Ok(())
}

/// Set the mandatory bits in CR0 and clear bits that are mandatory zero 
/// (Intel Manual: 24.8 Restrictions on VMX Operation)
fn set_cr0_bits() {
    let ia32_vmx_cr0_fixed0 = unsafe { msr::rdmsr(msr::IA32_VMX_CR0_FIXED0) };
    let ia32_vmx_cr0_fixed1 = unsafe { msr::rdmsr(msr::IA32_VMX_CR0_FIXED1) };

    let mut cr0 = unsafe { controlregs::cr0() };

    cr0 |= controlregs::Cr0::from_bits_truncate(ia32_vmx_cr0_fixed0 as usize);
    cr0 &= controlregs::Cr0::from_bits_truncate(ia32_vmx_cr0_fixed1 as usize);

    unsafe { controlregs::cr0_write(cr0) };
}

/// Set the mandatory bits in CR4 and clear bits that are mandatory zero 
/// (Intel Manual: 24.8 Restrictions on VMX Operation)
fn set_cr4_bits() {
    let ia32_vmx_cr4_fixed0 = unsafe { msr::rdmsr(msr::IA32_VMX_CR4_FIXED0) };
    let ia32_vmx_cr4_fixed1 = unsafe { msr::rdmsr(msr::IA32_VMX_CR4_FIXED1) };

    let mut cr4 = unsafe { controlregs::cr4() };

    cr4 |= controlregs::Cr4::from_bits_truncate(ia32_vmx_cr4_fixed0 as usize);
    cr4 &= controlregs::Cr4::from_bits_truncate(ia32_vmx_cr4_fixed1 as usize);

    unsafe { controlregs::cr4_write(cr4) };
}

