use alloc::sync::Arc;
use core::arch::asm;
use core::ptr::null_mut;

use crate::kdebug;
use crate::filesystem::devfs::devfs_register;
use self::kvm_dev::LockedKvmInode;
use vcpu::Vcpu;
use hypervisor::Hypervisor;
use crate::arch::x86_64::kvm::vmx::vcpu::VmxVcpu;
use crate::arch::KVMArch;
use crate::libs::mutex::Mutex;

mod kvm_dev;
mod vm_dev;
mod vcpu_dev;
pub mod vcpu;
pub mod hypervisor;

// pub const KVM_MAX_VCPUS:u32 = 255;
pub const GUEST_STACK_SIZE:usize = 1024;
pub const HOST_STACK_SIZE:usize = 0x1000 * 6;

static mut __KVM: *mut Arc<Mutex<Hypervisor>> = null_mut();

/// @brief 获取全局的根节点
#[inline(always)]
#[allow(non_snake_case)]
pub fn KVM() -> &'static Arc<Mutex<Hypervisor>> {
    unsafe {
        return __KVM.as_ref().unwrap();
    }
}
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

    match KVMArch::kvm_arch_cpu_supports_vm() {
        Ok(_) => { kdebug!("[+] CPU supports Intel VMX"); },
        Err(e) => {
            kdebug!("[-] CPU does not support Intel VMX: {:?}", e);
        }
    };
    
    KVMArch::kvm_arch_init().expect("kvm arch init");
    
    devfs_register("kvm", LockedKvmInode::new())
        .expect("Failed to register /dev/kvm");
    // let r = devfs_register("kvm", LockedKvmInode::new());
    // if r.is_err() {
    //     panic!("Failed to register /dev/kvm");
    // }
    let guest_stack = vec![0xCC; GUEST_STACK_SIZE];
    let host_stack = vec![0xCC; HOST_STACK_SIZE];
    let guest_rsp = guest_stack.as_ptr() as u64 + GUEST_STACK_SIZE as u64;
    let host_rsp = (host_stack.as_ptr() as u64) + HOST_STACK_SIZE  as u64;
    kdebug!("guest rsp: {:x}", guest_rsp);
    kdebug!("guest rip: {:x}", guest_code as *const () as u64);
    kdebug!("host rsp: {:x}", host_rsp);
    // let hypervisor = Hypervisor::new(1, host_rsp, 0).expect("Cannot create hypervisor");
    // let vcpu = VmxVcpu::new(1, Arc::new(Mutex::new(hypervisor)), host_rsp, guest_rsp,  guest_code as *const () as u64).expect("Cannot create VcpuData");
    // vcpu.virtualize_cpu().expect("Cannot virtualize cpu");
}

#[no_mangle]
pub extern "C" fn guest_code(){
    kdebug!("guest code");
    loop {
        unsafe {asm!(
            "mov rax, 0",
            "mov rcx, 0",
            "cpuid"
        );}
        kdebug!("guest code");
        unsafe {asm!("nop")};
    }
}
