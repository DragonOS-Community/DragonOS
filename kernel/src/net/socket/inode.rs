use crate::{
    filesystem::vfs::{
        fasync::FAsyncItem, file::File, FilePrivateData, FileType, IndexNode, InodeMode, Metadata,
        PollableInode,
    },
    libs::mutex::MutexGuard,
    syscall::user_access::{UserBufferReader, UserBufferWriter},
};
use alloc::{string::String, sync::Arc, vec::Vec};
use core::sync::atomic::Ordering;
use system_error::SystemError;

use super::{
    ioctl::{
        handle_netdev_mutation, handle_netdev_query, handle_siocgifconf, SIOCGIFCONF, SIOCGIFFLAGS,
        SIOCGIFHWADDR, SIOCGIFINDEX, SIOCGIFMTU, SIOCSIFFLAGS, SIOCSIFMTU,
    },
    Socket,
};

// Socket ioctl commands
const FIONREAD: u32 = 0x541B; // Get number of bytes available to read
const TIOCOUTQ: u32 = 0x5411; // Get output queue size

impl<T: Socket + 'static> IndexNode for T {
    fn fsnotify_watch_count(&self) -> Option<&core::sync::atomic::AtomicUsize> {
        Some(Socket::fsnotify_watch_counter(self))
    }

    fn open(
        &self,
        data: MutexGuard<FilePrivateData>,
        _: &crate::filesystem::vfs::file::FileFlags,
    ) -> Result<(), SystemError> {
        match &*data {
            FilePrivateData::SocketCreate => {
                self.open_file_counter().fetch_add(1, Ordering::Release);
                Ok(())
            }
            _ => Err(SystemError::ENXIO),
        }
    }

    fn close(&self, _: MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        // Only tear down the socket on the final close.
        if self.open_file_counter().fetch_sub(1, Ordering::AcqRel) == 1 {
            self.do_close()
        } else {
            Ok(())
        }
    }

    fn read_at(
        &self,
        _: usize,
        _: usize,
        buf: &mut [u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        // Drop the lock guard before calling self.read() to avoid holding the lock
        // across a potentially blocking or reentrant operation. This prevents deadlocks
        // and preemption issues.
        drop(data);
        self.read(buf)
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        buf: &[u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        // if buf.is_empty() {
        //     log::debug!(
        //         "Socket write_at: ZERO-LENGTH write, buf.len()={}, _len={}",
        //         buf.len(),
        //         _len
        //     );
        // }
        drop(data);
        self.write(buf)
    }

    fn write_user_at(
        &self,
        _offset: usize,
        len: usize,
        reader: &UserBufferReader<'_>,
        data: MutexGuard<FilePrivateData>,
    ) -> Result<Option<usize>, SystemError> {
        drop(data);
        self.send_user_buffer(reader, len, super::PMSG::empty(), None)
            .map(Some)
    }

    fn resize(&self, _len: usize) -> Result<(), SystemError> {
        Ok(())
    }
    fn page_cache(&self) -> Option<Arc<crate::filesystem::page_cache::PageCache>> {
        super::base::Socket::mmap_layout(self).map(|l| l.page_cache)
    }

    /// Validate the mmap request via the socket-specific handler. This checks
    /// ring size/offset bounds and updates the mapped count for teardown
    /// EBUSY accounting.
    fn mmap(&self, _start: usize, len: usize, offset: usize) -> Result<(), SystemError> {
        super::base::Socket::mmap_validate(self, len, offset)
    }

    fn fs(&self) -> Arc<dyn crate::filesystem::vfs::FileSystem> {
        super::base::Socket::mmap_fs(self)
            .unwrap_or_else(|| unreachable!("Socket does not have a file system"))
    }

    fn try_fs(&self) -> Option<Arc<dyn crate::filesystem::vfs::FileSystem>> {
        None
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

    fn metadata(&self) -> Result<crate::filesystem::vfs::Metadata, SystemError> {
        let mut md = Metadata::new(FileType::Socket, InodeMode::from_bits_truncate(0o755));
        md.inode_id = self.socket_inode_id();
        md.mode |= InodeMode::S_IFSOCK;
        Ok(md)
    }

    /// 这里应该实现 通用 Socket 作为 IndexNode 的 ioctl 选项
    /// 对于协议特定的 ioctl 选项实现，请在各个 Socket impl trait 内实现
    ///
    /// ## 层级结构
    ///
    /// `dyn IndexNode::ioctl` -> `impl IndexNode for T: Socket` -> `dyn Socket::ioctl`
    ///
    /// Socket trait 的 ioctl 覆盖了 IndexNode 这一层的调用，但由于 `impl IndexNode for T: Socket`，
    /// 我们先调用在 IndexNode 这一层为 Socket 默认实现的 ioctl，再调用 `Socket` trait 内
    /// 的 ioctl
    fn ioctl(
        &self,
        cmd: u32,
        data: usize,
        private_data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        match cmd {
            SIOCGIFCONF => handle_siocgifconf(self.netns(), data),
            SIOCGIFINDEX | SIOCGIFFLAGS | SIOCGIFMTU | SIOCGIFHWADDR => {
                handle_netdev_query(self.netns(), cmd, data)
            }
            SIOCSIFFLAGS | SIOCSIFMTU => handle_netdev_mutation(self.netns(), cmd, data),
            FIONREAD /* TIOCINQ */ => {
                // Get number of bytes available to read
                let bytes_available = self.recv_bytes_available();
                let mut writer =
                    UserBufferWriter::new(data as *mut u8, core::mem::size_of::<i32>(), true)?;
                let to_write = core::cmp::min(bytes_available, i32::MAX as usize) as i32;
                writer.buffer_protected(0)?.write_one::<i32>(0, &to_write)?;
                Ok(0)
            }
            TIOCOUTQ => {
                // Get number of bytes available to write
                let bytes_available = self.send_bytes_available()?;
                let mut writer =
                    UserBufferWriter::new(data as *mut u8, core::mem::size_of::<i32>(), true)?;
                let to_write = core::cmp::min(bytes_available, i32::MAX as usize) as i32;
                writer.buffer_protected(0)?.write_one::<i32>(0, &to_write)?;
                Ok(0)
            }
            _ => {
                // 透穿调用子协议栈的ioctl
                Socket::ioctl(self, cmd, data, &private_data)
            }
        }
    }

    fn as_socket(&self) -> Option<&dyn Socket> {
        Some(self)
    }
}

impl<T: Socket + 'static> PollableInode for T {
    fn poll(&self, _: &FilePrivateData) -> Result<usize, SystemError> {
        Ok(self.check_io_event().bits() as usize)
    }

    fn add_epitem(
        &self,
        epitem: Arc<crate::filesystem::epoll::EPollItem>,
        _: &FilePrivateData,
    ) -> Result<(), SystemError> {
        self.epoll_items().add(epitem);
        return Ok(());
    }

    fn remove_epitem(
        &self,
        epitm: &Arc<crate::filesystem::epoll::EPollItem>,
        _: &FilePrivateData,
    ) -> Result<(), SystemError> {
        let _ = self.epoll_items().remove(epitm);
        return Ok(());
    }

    fn add_fasync(&self, fasync_item: FAsyncItem, _: &FilePrivateData) -> Result<(), SystemError> {
        self.fasync_items().add(fasync_item);
        Ok(())
    }

    fn remove_fasync(
        &self,
        file: &alloc::sync::Weak<File>,
        _: &FilePrivateData,
    ) -> Result<(), SystemError> {
        self.fasync_items().remove(file);
        Ok(())
    }

    fn release_fasync(&self, file: &File, _: &FilePrivateData) -> Result<(), SystemError> {
        self.fasync_items().remove_file(file);
        Ok(())
    }
}
