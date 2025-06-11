use crate::filesystem::vfs::{IndexNode, PollableInode};
use alloc::{string::String, sync::Arc, vec::Vec};
use system_error::SystemError;

use super::Socket;

impl<T: Socket + 'static> IndexNode for T {
    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        buf: &mut [u8],
        _data: crate::libs::spinlock::SpinLockGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        self.read(buf)
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        buf: &[u8],
        _data: crate::libs::spinlock::SpinLockGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        self.write(buf)
    }

    fn fs(&self) -> Arc<dyn crate::filesystem::vfs::FileSystem> {
        unreachable!("Socket does not have a file system")
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn list(&self) -> Result<Vec<String>, SystemError> {
        Err(SystemError::ENOTDIR)
    }

    fn as_pollable_inode(&self) -> Result<&dyn PollableInode, SystemError> {
        Ok(self)
    }

    fn as_socket(&self) -> Option<&dyn Socket> {
        Some(self)
    }
}
