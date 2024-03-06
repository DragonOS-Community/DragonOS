use system_error::SystemError;

use crate::{
    filesystem::vfs::{
        file::{File, FileMode},
        ROOT_INODE,
    },
    process::{Pid, ProcessManager},
};

use super::{ProcessFlags, __PROCESS_MANAGEMENT_INIT_DONE};

pub fn current_pcb_flags() -> ProcessFlags {
    if unsafe { !__PROCESS_MANAGEMENT_INIT_DONE } {
        return ProcessFlags::empty();
    }
    return ProcessManager::current_pcb().flags().clone();
}

pub fn current_pcb_preempt_count() -> usize {
    if unsafe { !__PROCESS_MANAGEMENT_INIT_DONE } {
        return 0;
    }
    return ProcessManager::current_pcb().preempt_count();
}

/// @brief 初始化pid=1的进程的stdio
pub fn stdio_init() -> Result<(), SystemError> {
    if ProcessManager::current_pcb().pid() != Pid(1) {
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
