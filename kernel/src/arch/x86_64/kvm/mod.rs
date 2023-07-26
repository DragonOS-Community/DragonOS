
use crate::{
    kerror,
    // libs::spinlock::{SpinLock, SpinLockGuard},
    syscall::SystemError,
};

fn kvm_arch_dev_ioctl(cmd: u32, arg: usize) -> Result<usize, SystemError> {
    match cmd {
        _ => {
            kerror!("unknown kvm ioctl cmd: {}", cmd);
            return Err(SystemError::EINVAL);
        }
    }
}