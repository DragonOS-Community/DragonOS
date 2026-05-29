use alloc::{string::ToString, sync::Arc};

use system_error::SystemError;
use unified_init::macros::unified_init;

use crate::{
    init::initcall::INITCALL_CORE,
    process::{kthread::KernelThreadClosure, kthread::KernelThreadMechanism, ProcessControlBlock},
    time::{sleep::nanosleep, PosixTimeSpec},
};

use super::mount::list_unique_mounted_superblocks;

static mut VFS_WRITEBACK_THREAD: Option<Arc<ProcessControlBlock>> = None;

#[unified_init(INITCALL_CORE)]
fn vfs_writeback_thread_init() -> Result<(), SystemError> {
    let closure =
        KernelThreadClosure::StaticEmptyClosure((&(vfs_writeback_thread as fn() -> i32), ()));
    let pcb = KernelThreadMechanism::create_and_run(closure, "vfs_writeback".to_string())
        .ok_or("")
        .expect("create vfs_writeback thread failed");
    unsafe {
        VFS_WRITEBACK_THREAD = Some(pcb);
    }
    Ok(())
}

fn vfs_writeback_thread() -> i32 {
    loop {
        for mount in list_unique_mounted_superblocks() {
            if let Err(e) = mount.try_sync_fs_with_umount_read(false) {
                log::warn!("vfs_writeback: sync_fs failed: {:?}", e);
            }
        }

        let _ = nanosleep(PosixTimeSpec::new(5, 0));
    }
}
