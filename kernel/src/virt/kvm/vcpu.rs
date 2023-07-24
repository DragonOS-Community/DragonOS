use crate::mm::allocator::kernel_allocator::KernelAllocator;
use x86::{controlregs, msr};
use raw_cpuid::CpuId;
use x86;
use crate::{kdebug, printk_color, GREEN, BLACK};
use alloc::boxed::Box;
use alloc::alloc::Global;
use alloc::sync::Arc;
use core::ptr;

use crate::arch::MMArch;
use crate::mm::MemoryManagementArch;
use crate::mm::{VirtAddr};
use crate::syscall::SystemError;
use crate::virt::kvm::hypervisor::Hypervisor;

// KERNEL_ALLOCATOR
pub const PAGE_SIZE: usize = 0x1000;

#[repr(C, align(4096))]
pub struct VmxonRegion {
    pub revision_id: u32,
    pub data: [u8; PAGE_SIZE - 4],
}

#[repr(C, align(4096))]
pub struct VMCSRegion {
    pub revision_id: u32,
    pub abort_indicator: u32, 
    pub data: [u8; PAGE_SIZE - 8],
}

pub struct VcpuData {
    /// The virtual and physical address of the Vmxon naturally aligned 4-KByte region of memory
    pub vmxon_region: Box<VmxonRegion>,
    pub vmxon_region_physical_address: u64,  // vmxon需要该地址
    /// The virtual and physical address of the Vmcs naturally aligned 4-KByte region of memory
    pub vmcs_region: Box<VMCSRegion>,
    pub vmcs_region_physical_address: u64,  // vmptrld, vmclear需要该地址
}

pub struct Vcpu {
    /// The index of the processor.
    index: u32,
    data: Box<VcpuData>, 
    hypervisor: Arc<Hypervisor>,		/* parent KVM */
}

impl VcpuData {
    pub fn new() -> Result<Box<Self>, SystemError> {
        let instance = Self {
            // try_new_zeroed_in 创建一个具有未初始化内容的新 Box，使用提供的分配器中的 0 字节填充内存，如果分配失败，则返回错误
            // assume_init 由调用者负责确保值确实处于初始化状态
            vmxon_region: unsafe { 
                match Box::try_new_zeroed_in(Global) {
                    Ok(zero) => zero.assume_init(),
                    Err(_) => panic!("Try new zeroed fail!"),
                }
            },
            vmxon_region_physical_address: 0,
            vmcs_region: unsafe { 
                match Box::try_new_zeroed_in(Global) {
                    Ok(zero) => zero.assume_init(),
                    Err(_) => panic!("Try new zeroed fail!"),
                }
            },
            vmcs_region_physical_address: 0,
        };

        let mut instance = Box::new(instance);
        printk_color!(GREEN, BLACK, "[+] init_vmxon_region\n");
        instance.init_vmxon_region()?;
        instance.init_vmcs_region(0,0,0)?;
        Ok(instance)
    }

    // Allocate a naturally aligned 4-KByte VMXON region of memory to enable VMX operation
    // (Intel Manual: 25.11.5 VMXON Region)
    pub fn init_vmxon_region(&mut self) -> Result<(), SystemError> {
        // FIXME: virt_2_phys的转换正确性存疑
        let vaddr = VirtAddr::new(self.vmxon_region.as_ref() as *const _ as _);
        self.vmxon_region_physical_address =  unsafe {MMArch::virt_2_phys(vaddr).unwrap().data() as u64};
        // virt_2_phys(self.vmxon_region.as_ref() as *const _ as _) as u64;

        if self.vmxon_region_physical_address == 0 {
            return Err(SystemError::EFAULT);
        }

        kdebug!("[+] VMXON Region Virtual Address: {:p}", self.vmxon_region);
        kdebug!("[+] VMXON Region Physical Addresss: 0x{:x}", self.vmxon_region_physical_address);

        self.vmxon_region.revision_id = get_vmcs_revision_id();
        // self.vmxon_region.as_mut().revision_id.set_bit(31, false);

        Ok(())
    }

    pub fn init_vmcs_region(&mut self, guest_rsp: u32, guest_rip:u32, is_pt_allowed: u32) -> Result<(), SystemError> {
        let vaddr = VirtAddr::new(self.vmcs_region.as_ref() as *const _ as _);
        self.vmcs_region_physical_address =  unsafe {MMArch::virt_2_phys(vaddr).unwrap().data() as u64};
        // virt_2_phys(self.vmxon_region.as_ref() as *const _ as _) as u64;

        if self.vmcs_region_physical_address == 0 {
            return Err(SystemError::EFAULT);
        }
        self.vmcs_region.revision_id = get_vmcs_revision_id();
        Ok(())
    }
}

impl Vcpu {
    pub fn new(index: u32, hypervisor: Arc<Hypervisor>) -> Result<Self, SystemError> {
        kdebug!("Creating processor {}", index);
        Ok (Self {
            index,
            data: VcpuData::new()?, 
            hypervisor: hypervisor,
        })
    }

    /// Virtualize the CPU
    pub fn virtualize_cpu(&self) -> Result<(), SystemError> {
        match has_intel_vmx_support() {
            Ok(_) => { kdebug!("[+] CPU supports Intel VMX"); },
            Err(e) => {
                kdebug!("[-] CPU does not support Intel VMX: {:?}", e);
                return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
            }
        };
        
        match enable_vmx_operation(){
            Ok(_) => { kdebug!("[+] Enabling Virtual Machine Extensions (VMX)"); },
            Err(e) => {
                kdebug!("[-] VMX operation is not supported on this processor.");
                return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
            }
        }

        vmxon(self.data.vmxon_region_physical_address)?;
        kdebug!("[+] VMXON successful!");

        Ok(())
    }

    pub fn devirtualize_cpu(&self) -> Result<(), SystemError> {
        vmxoff()?;
        Ok(())
    }

    /// Gets the index of the current logical/virtual processor
    pub fn id(&self) -> u32 {
        self.index
    }
}

/// Enable VMX operation.
pub fn vmxon(vmxon_pa: u64) -> Result<(), SystemError> {
    match unsafe { x86::bits64::vmx::vmxon(vmxon_pa) } {
        Ok(_) => Ok(()),
        Err(_) => Err(SystemError::EVMXONFailed),
    }
}

/// Disable VMX operation.
pub fn vmxoff() -> Result<(), SystemError> {
    match unsafe { x86::bits64::vmx::vmxoff() } {
        Ok(_) => Ok(()),
        Err(_) => Err(SystemError::EVMXOFFFailed),
    }
}

/// Get the Virtual Machine Control Structure revision identifier (VMCS revision ID) 
// (Intel Manual: 25.11.5 VMXON Region)
pub fn get_vmcs_revision_id() -> u32 {
    unsafe { (msr::rdmsr(msr::IA32_VMX_BASIC) as u32) & 0x7FFF_FFFF }
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
