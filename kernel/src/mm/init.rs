use core::{fmt::Write, sync::atomic::Ordering};

use log::info;

use crate::{
    arch::MMArch,
    driver::serial::serial8250::send_to_default_serial8250_port,
    filesystem::procfs::kmsg::kmsg_init,
    ipc::shm::shm_manager_init,
    libs::printk::PrintkWriter,
    mm::{
        allocator::slab::slab_init,
        mmio_buddy::mmio_init,
        page::{page_manager_init, page_reclaimer_init},
    },
};

use super::MemoryManagementArch;

#[atomic_enum]
#[derive(PartialEq, Eq)]
pub enum MMInitStatus {
    NotInit,
    Initializing,
    Initialized,
}
/// 内存管理的初始化状态
static MM_INIT: AtomicMMInitStatus = AtomicMMInitStatus::new(MMInitStatus::NotInit);

#[inline(never)]
pub unsafe fn mm_init() {
    send_to_default_serial8250_port("mm_init\n\0".as_bytes());
    PrintkWriter
        .write_fmt(format_args!("mm_init() called\n"))
        .unwrap();
    // printk_color!(GREEN, BLACK, "mm_init() called\n");

    if MM_INIT
        .compare_exchange(
            MMInitStatus::NotInit,
            MMInitStatus::Initializing,
            Ordering::SeqCst,
            Ordering::SeqCst,
        )
        .is_err()
    {
        send_to_default_serial8250_port("mm_init err\n\0".as_bytes());
        panic!("mm_init() can only be called once");
    }

    MMArch::init();

    // init slab
    slab_init();

    // enable mmio
    mmio_init();
    // enable KMSG
    kmsg_init();
    // enable PAGE_MANAGER
    page_manager_init();
    // enable SHM_MANAGER
    shm_manager_init();
    // enable PAGE_RECLAIMER
    page_reclaimer_init();

    MM_INIT
        .compare_exchange(
            MMInitStatus::Initializing,
            MMInitStatus::Initialized,
            Ordering::SeqCst,
            Ordering::SeqCst,
        )
        .unwrap();
    MMArch::arch_post_init();
    info!("mm init done.");
}

/// 获取内存管理的初始化状态
pub fn mm_init_status() -> MMInitStatus {
    MM_INIT.load(Ordering::SeqCst)
}
