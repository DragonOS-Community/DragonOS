use core::arch::x86_64;
use raw_cpuid::CpuId;
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
    devfs_register("kvm", LockedKvmInode::new())
        .expect("Failed to register /dev/kvm");
}

// fn kvm_dev_ioctl_create_vm(data: usize) {
//     let kvm: Arc<Kvm> = Arc::new(Kvm(
//         sys_fd::-1,
//         vm_fd::-1,
//     ));
// }