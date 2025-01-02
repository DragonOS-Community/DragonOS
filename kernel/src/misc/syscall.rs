use system_error::SystemError;

use crate::syscall::Syscall;

use super::reboot::do_sys_reboot;

impl Syscall {
    pub fn reboot(magic1: u32, magic2: u32, cmd: u32, arg: usize) -> Result<usize, SystemError> {
        do_sys_reboot(magic1, magic2, cmd, arg).map(|_| 0)
    }
}
