use core::{ffi::c_void, mem::ManuallyDrop, ptr::null_mut};

use crate::{
    filesystem::vfs::{
        file::{File, FileMode},
        ROOT_INODE,
    },
    process::{Pid, ProcessManager},
    syscall::SystemError,
};

/// @brief 初始化pid=1的进程的stdio
pub fn init_stdio() -> Result<(), SystemError> {
    if ProcessManager::current_pcb().basic().pid() != Pid(1) {
        return Err(SystemError::EPERM);
    }
    let tty_inode = ROOT_INODE()
        .lookup("/dev/tty0")
        .expect("Init stdio: can't find tty0");
    let stdin =
        File::new(tty_inode.clone(), FileMode::O_RDONLY).expect("Init stdio: can't create stdin");
    let stdout =
        File::new(tty_inode.clone(), FileMode::O_WRONLY).expect("Init stdio: can't create stdout");
    let stderr = File::new(tty_inode.clone(), FileMode::O_WRONLY | FileMode::O_SYNC)
        .expect("Init stdio: can't create stderr");

    /*
       按照规定，进程的文件描述符数组的前三个位置，分别是stdin, stdout, stderr
    */
    assert_eq!(
        ProcessManager::current_pcb()
            .fd_table()
            .write()
            .alloc_fd(stdin, None)
            .unwrap(),
        0
    );
    assert_eq!(
        ProcessManager::current_pcb()
            .fd_table()
            .write()
            .alloc_fd(stdout, None)
            .unwrap(),
        1
    );
    assert_eq!(
        ProcessManager::current_pcb()
            .fd_table()
            .write()
            .alloc_fd(stderr, None)
            .unwrap(),
        2
    );
    return Ok(());
}
