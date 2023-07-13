use core::arch::x86_64;
use crate::kdebug;
use crate::filesystem::devfs::{DevFS, DeviceINode, devfs_register};
pub use self::kvm_dev::LockedKvmInode;
mod kvm_dev;

#[no_mangle]
pub extern "C" fn kvm_init() {
    kdebug!("kvm init");
    // let r = devfs_register("kvm", LockedKvmInode::new());
    // if r.is_err() {
    //     panic!("Failed to register /dev/kvm");
    // }
    devfs_register("kvm", LockedKvmInode::new())
        .expect("Failed to register /dev/kvm");
}