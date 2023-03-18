use core::fmt::Debug;

use crate::syscall::SystemError;

pub trait Socket: Sync + Send + Debug {
    fn read(&self, buf: &mut [u8]) -> Result<usize, SystemError>;

    fn write(&self, buf: &[u8]) -> Result<usize, SystemError>;
}
