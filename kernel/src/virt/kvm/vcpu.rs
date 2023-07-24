use crate::mm::allocator::kernel_allocator::KernelAllocator;
use x86::{controlregs, msr};
use x86;
use crate::{kdebug, printk_color, GREEN, BLACK};
use alloc::boxed::Box;
use alloc::alloc::Global;

use crate::arch::MMArch;
use crate::mm::MemoryManagementArch;
use crate::mm::{VirtAddr};
use crate::syscall::SystemError;

// KERNEL_ALLOCATOR
pub const PAGE_SIZE: usize = 0x1000;

#[repr(C, align(4096))]
pub struct VmxonRegion {
    pub revision_id: u32,
    pub data: [u8; PAGE_SIZE - 4],
}

pub struct VcpuData {
    /// The virtual and physical address of the Vmxon naturally aligned 4-KByte region of memory
    pub vmxon_region: Box<VmxonRegion>,
    pub vmxon_region_physical_address: u64,
}

pub struct Vcpu {
    /// The index of the processor.
    index: u32,
    data: Box<VcpuData>, 
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
        };

        let mut instance = Box::new(instance);
        printk_color!(GREEN, BLACK, "[+] init_vmxon_region\n");
        instance.init_vmxon_region()?;
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

        vmxon(self.vmxon_region_physical_address)?;
        kdebug!("[+] VMXON successful!");

        Ok(())
    }
}

impl Vcpu {
    pub fn new(index: u32) -> Result<Self, SystemError> {
        kdebug!("Creating processor {}", index);

        Ok (Self {
            index,
            data: VcpuData::new().expect("VcpuData"), // virtualize CPU
        })
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
