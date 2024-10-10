use system_error::SystemError;

use crate::{
    driver::tty::virtual_terminal::vc_manager,
    filesystem::vfs::{
        file::{File, FileMode},
        ROOT_INODE,
    },
    process::{Pid, ProcessManager},
};

/// @brief 初始化pid=1的进程的stdio
pub fn stdio_init() -> Result<(), SystemError> {
    if ProcessManager::current_pcb().pid() != Pid(1) {
        return Err(SystemError::EPERM);
    }
    let tty_path = format!(
        "/dev/{}",
        vc_manager()
            .current_vc_tty_name()
            .expect("Init stdio: can't get tty name")
    );
    let tty_inode = ROOT_INODE()
        .lookup(&tty_path)
        .unwrap_or_else(|_| panic!("Init stdio: can't find {}", tty_path));

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
