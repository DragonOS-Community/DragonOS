use core::arch::x86_64;
use raw_cpuid::CpuId;
use x86::{controlregs, msr};
use crate::kdebug;
use crate::filesystem::devfs::{DevFS, DeviceINode, devfs_register};
pub use self::kvm_dev::LockedKvmInode;
use crate::syscall::SystemError;

mod kvm_dev;

pub const KVM_MAX_VCPUS:u32 = 255;

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

// struct Kvm {
//     sys_fd: u32,	/* For system ioctls(), i.e. /dev/kvm */
// 	vm_fd: u32,  	/* For VM ioctls() */
//     timerid: u32,   /* Posix timer for interrupts */

//     nr_vcpus: u32,  /* Number of cpus to run */
//     vcpu: Box<[kvm_vcpu]>,

//     mem_slots: u32, /* for KVM_SET_USER_MEMORY_REGION */
//     ram_size: u64,  /* Guest memory size, in bytes */
//     ram_start: *u64,
//     ram_pagesize: u64,
//     mem_banks_lock: SpinLock<()>,
//     // mem_banks: Box<[kvm_mem_bank]>,

//     vm_state: u32,
// }

// struct Kvm_vcpu {
//     kvm: Arc<Kvm>,		/* parent KVM */
//     cpu_id: u32,        /* CPU id */
//     vcpu_fd: u32,       /* For VCPU ioctls() */
// 	pthread_t: thread,		/* VCPU thread */

// 	kvm_run: Arc<Kvm_run>,
// 	// struct kvm_cpu_task	*task;
    
// 	struct kvm_regs		regs;
// 	struct kvm_sregs	sregs;
// 	struct kvm_fpu		fpu;

// 	struct kvm_msrs		*msrs;		/* dynamically allocated */

//     // vcpu states
// 	is_running: u8, 
// 	paused: u8, 
// 	needs_nmi: u8,

// 	struct kvm_coalesced_mmio_ring	*ring;
// };

// struct kvm_arch{

// }

#[no_mangle]
pub extern "C" fn kvm_init() {
    kdebug!("kvm init");
    // let r = devfs_register("kvm", LockedKvmInode::new());
    // if r.is_err() {
    //     panic!("Failed to register /dev/kvm");
    // }
    has_intel_vmx_support().expect("No Intel VMX support");
    enable_vmx_operation().expect("Cannot enable vmx operation");
    devfs_register("kvm", LockedKvmInode::new())
        .expect("Failed to register /dev/kvm");
}

// fn kvm_dev_ioctl_create_vm(data: usize) {
//     let kvm: Arc<Kvm> = Arc::new(Kvm(
//         sys_fd::-1,
//         vm_fd::-1,
//     ));
// }