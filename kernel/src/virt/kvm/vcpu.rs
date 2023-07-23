use crate::mm::allocator::kernel_allocator::KernelAllocator;
use x86::{controlregs, msr};
use x86;
use crate::{kdebug, printk_color, GREEN, BLACK};
use alloc::boxed::Box;
use alloc::alloc::Global;
use crate::mm::virt_2_phys;
use crate::syscall::SystemError;
use core::ptr;


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

impl VcpuData {
    pub fn new() -> Result<Box<Self>, SystemError> {
        kdebug!("[+] VcpuData::new\n");
        let instance = Self {
            // try_new_zeroed_in 创建一个具有未初始化内容的新 Box，使用提供的分配器中的 0 字节填充内存，如果分配失败，则返回错误
            // assume_init 由调用者负责确保值确实处于初始化状态
            vmxon_region: unsafe {
                let mut b = Box::new_uninit();
                unsafe { ptr::write_bytes(b.as_mut_ptr(), 0, 0x1000) };
                let boxed_array: Box<VmxonRegion> = unsafe { b.assume_init() };
                boxed_array
            },
            vmxon_region_physical_address: 0,
        };

        printk_color!(GREEN, BLACK, "[+] instance!\n");
        let mut instance = Box::new(instance);
                
        kdebug!("[+] init_vmxon_region");
        instance.init_vmxon_region()?;
        Ok(instance)
    }

    // Allocate a naturally aligned 4-KByte VMXON region of memory to enable VMX operation
    // (Intel Manual: 25.11.5 VMXON Region)
    pub fn init_vmxon_region(&mut self) -> Result<(), SystemError> {
        self.vmxon_region_physical_address = virt_2_phys(self.vmxon_region.as_ref() as *const _ as _) as u64;

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


/// Enable VMX operation.
pub fn vmxon(vmxon_pa: u64) -> Result<(), SystemError> {
    match unsafe { x86::bits64::vmx::vmxon(vmxon_pa) } {
        Ok(_) => Ok(()),
        Err(_) => Err(SystemError::EVMXONFailed),
    }
}


/// Get the Virtual Machine Control Structure revision identifier (VMCS revision ID) 
// (Intel Manual: 25.11.5 VMXON Region)
pub fn get_vmcs_revision_id() -> u32 {
    unsafe { (msr::rdmsr(msr::IA32_VMX_BASIC) as u32) & 0x7FFF_FFFF }
}
