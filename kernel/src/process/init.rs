//! 这个文件内放置初始内核线程的代码。

use alloc::string::String;

use crate::{
    arch::process::arch_switch_to_user,
    driver::{
        disk::ahci::ahci_init, net::e1000e::e1000e::e1000e_init, virtio::virtio::virtio_probe,
    },
    filesystem::vfs::core::mount_root_fs,
    kdebug, kerror,
    net::net_core::net_init,
    process::{kthread::KernelThreadMechanism, process::stdio_init},
};

pub fn initial_kernel_thread() -> i32 {
    KernelThreadMechanism::init_stage2();
    // 由于目前加锁，速度过慢，所以先不开启双缓冲
    // scm_enable_double_buffer().expect("Failed to enable double buffer");
    stdio_init().expect("Failed to initialize stdio");

    ahci_init().expect("Failed to initialize AHCI");

    mount_root_fs().expect("Failed to mount root fs");

    virtio_probe();
    e1000e_init();
    net_init().unwrap_or_else(|err| {
        kerror!("Failed to initialize network: {:?}", err);
    });

    kdebug!("initial kernel thread done.");

    switch_to_user();

    loop {}
}

/// 切换到用户态
fn switch_to_user() {
    let path = String::from("/bin/DragonReach");
    let argv = vec![String::from("/bin/DragonReach")];
    let envp = vec![String::from("PATH=/")];

    unsafe { arch_switch_to_user(path, argv, envp) };
}
