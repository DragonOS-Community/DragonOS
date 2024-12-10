use self::kvm_dev::LockedKvmInode;
use crate::arch::KVMArch;
use crate::filesystem::devfs::devfs_register;

use crate::libs::mutex::Mutex;
use alloc::vec::Vec;
use log::debug;
use vm::Vm;

pub mod host_mem;
mod kvm_dev;
pub mod vcpu;
mod vcpu_dev;
pub mod vm;
mod vm_dev;

// pub const KVM_MAX_VCPUS:u32 = 255;
// pub const GUEST_STACK_SIZE:usize = 1024;
// pub const HOST_STACK_SIZE:usize = 0x1000 * 6;

/// @brief 获取全局的VM list
pub static VM_LIST: Mutex<Vec<Vm>> = Mutex::new(Vec::new());

pub fn push_vm(id: usize) -> Result<(), ()> {
    let mut vm_list = VM_LIST.lock();
    if vm_list.iter().any(|x| x.id == id) {
        debug!("push_vm: vm {} already exists", id);
        Err(())
    } else {
        vm_list.push(Vm::new(id).unwrap());
        Ok(())
    }
}

pub fn remove_vm(id: usize) -> Vm {
    let mut vm_list = VM_LIST.lock();
    match vm_list.iter().position(|x| x.id == id) {
        None => {
            panic!("VM[{}] not exist in VM LIST", id);
        }
        Some(idx) => vm_list.remove(idx),
    }
}

pub fn update_vm(id: usize, new_vm: Vm) {
    remove_vm(id);
    let mut vm_list = VM_LIST.lock();
    vm_list.push(new_vm);
}

pub fn vm(id: usize) -> Option<Vm> {
    let vm_list = VM_LIST.lock();
    vm_list.iter().find(|&x| x.id == id).cloned()
}

#[inline(never)]
pub fn kvm_init() {
    debug!("kvm init");

    match KVMArch::kvm_arch_cpu_supports_vm() {
        Ok(_) => {
            debug!("[+] CPU supports Intel VMX");
        }
        Err(e) => {
            debug!("[-] CPU does not support Intel VMX: {:?}", e);
        }
    };

    KVMArch::kvm_arch_init().expect("kvm arch init");

    devfs_register("kvm", LockedKvmInode::new()).expect("Failed to register /dev/kvm");
    // let r = devfs_register("kvm", LockedKvmInode::new());
    // if r.is_err() {
    //     panic!("Failed to register /dev/kvm");
    // }
    // let guest_stack = vec![0xCC; GUEST_STACK_SIZE];
    // let host_stack = vec![0xCC; HOST_STACK_SIZE];
    // let guest_rsp = guest_stack.as_ptr() as u64 + GUEST_STACK_SIZE as u64;
    // let host_rsp = (host_stack.as_ptr() as u64) + HOST_STACK_SIZE  as u64;
    // debug!("guest rsp: {:x}", guest_rsp);
    // debug!("guest rip: {:x}", guest_code as *const () as u64);
    // debug!("host rsp: {:x}", host_rsp);
    // let hypervisor = Hypervisor::new(1, host_rsp, 0).expect("Cannot create hypervisor");
    // let vcpu = VmxVcpu::new(1, Arc::new(Mutex::new(hypervisor)), host_rsp, guest_rsp,  guest_code as *const () as u64).expect("Cannot create VcpuData");
    // vcpu.virtualize_cpu().expect("Cannot virtualize cpu");
}
