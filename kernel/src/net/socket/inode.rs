use crate::{
    arch::MMArch,
    filesystem::vfs::{
        fasync::FAsyncItem, file::File, merge_metadata_masked, FilePrivateData, FileType,
        IndexNode, InodeId, InodeMode, Metadata, MetadataUpdate, PollableInode, SetMetadataMask,
    },
    libs::mutex::MutexGuard,
    libs::spinlock::SpinLock,
    mm::MemoryManagementArch,
    process::ProcessManager,
    syscall::user_access::{UserBufferReader, UserBufferWriter},
};
use alloc::{string::String, sync::Arc, vec::Vec};
use core::sync::atomic::Ordering;
use system_error::SystemError;

use super::{
    ioctl::{
        handle_netdev_mutation, handle_netdev_query, handle_siocgifconf, SIOCGIFCONF, SIOCGIFFLAGS,
        SIOCGIFHWADDR, SIOCGIFINDEX, SIOCGIFMTU, SIOCGIFTXQLEN, SIOCSIFFLAGS, SIOCSIFMTU,
    },
    Socket,
};

// Socket ioctl commands
const FIONREAD: u32 = 0x541B; // Get number of bytes available to read
const TIOCOUTQ: u32 = 0x5411; // Get output queue size

/// Mutable inode attributes shared by every socket protocol.
///
/// Linux stores these fields directly in the sockfs inode. Keeping them on the
/// socket object gives the generic VFS `fstat`/`fchmod` paths a single owner
/// without a global inode-id table.
#[derive(Debug)]
pub struct SocketInodeMetadata {
    metadata: SpinLock<Metadata>,
}

impl SocketInodeMetadata {
    pub fn new(inode_id: InodeId) -> Self {
        let (uid, gid) = current_socket_owner();
        let mut metadata =
            Metadata::new(FileType::Socket, InodeMode::S_IFSOCK | InodeMode::S_IRWXUGO);
        metadata.inode_id = inode_id;
        // Linux places sockets on the pseudo `sockfs`, whose block size is
        // PAGE_SIZE, so `i_blocksize()` (and therefore `st_blksize`) is a page.
        metadata.blk_size = MMArch::PAGE_SIZE;
        metadata.uid = uid;
        metadata.gid = gid;
        Self {
            metadata: SpinLock::new(metadata),
        }
    }

    pub fn snapshot(&self, inode_id: InodeId) -> Metadata {
        let mut metadata = self.metadata.lock().clone();
        metadata.inode_id = inode_id;
        metadata
    }

    /// Current socket mode, including the `S_IFSOCK` type bits.
    pub fn mode(&self) -> InodeMode {
        self.metadata.lock().mode
    }

    pub fn set_metadata_masked(&self, requested: &Metadata, mask: SetMetadataMask) {
        let mut metadata = self.metadata.lock();
        merge_metadata_masked(&mut metadata, requested, mask);
    }

    /// Reinitialize the owner of an inode that is being published by
    /// `accept(2)`.
    ///
    /// Linux allocates the accepted socket's sockfs inode inside the accepting
    /// task (`sock_alloc()` in `net/socket.c`), so `i_uid`/`i_gid` come from
    /// the acceptor's fsuid/fsgid. Protocols that materialize the passive peer
    /// on `connect()` (Unix streams) must refresh the owner before exposing the
    /// child descriptor.
    pub fn reset_owner_to_current(&self) {
        let (uid, gid) = current_socket_owner();
        let mut metadata = self.metadata.lock();
        metadata.uid = uid;
        metadata.gid = gid;
    }

    /// Run the caller's check-and-compute step while holding the metadata
    /// lock, so permission checks and the committed mode cannot race.
    ///
    /// The callback runs under a spinlock and must therefore stay in-memory:
    /// it may not sleep, block, or allocate.
    pub fn update_metadata_masked(
        &self,
        update: &mut MetadataUpdate<'_>,
    ) -> Result<SetMetadataMask, SystemError> {
        let mut metadata = self.metadata.lock();
        let (requested, mask) = update(&metadata)?;
        merge_metadata_masked(&mut metadata, &requested, mask);
        Ok(mask)
    }
}

fn current_socket_owner() -> (usize, usize) {
    if ProcessManager::initialized() {
        let cred = ProcessManager::current_cred();
        (cred.fsuid.data(), cred.fsgid.data())
    } else {
        (0, 0)
    }
}

impl<T: Socket + 'static> IndexNode for T {
    fn fsnotify_watch_count(&self) -> Option<&core::sync::atomic::AtomicUsize> {
        Some(Socket::fsnotify_watch_counter(self))
    }

    fn open(
        &self,
        data: MutexGuard<FilePrivateData>,
        flags: &crate::filesystem::vfs::file::FileFlags,
    ) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
        match &*data {
            FilePrivateData::SocketCreate => {
                // The new open file description owns the mode. In particular,
                // accept does not inherit the listener's O_NONBLOCK.
                self.set_nonblocking(
                    flags.contains(crate::filesystem::vfs::file::FileFlags::O_NONBLOCK),
                );
                self.open_file_counter().fetch_add(1, Ordering::Release);
                Ok(())
            }
            _ => Err(SystemError::ENXIO.into()),
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
    ) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
        // Drop the lock guard before calling self.read() to avoid holding the lock
        // across a potentially blocking or reentrant operation. This prevents deadlocks
        // and preemption issues.
        drop(data);
        self.read(buf).map_err(Into::into)
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        buf: &[u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
        // if buf.is_empty() {
        //     log::debug!(
        //         "Socket write_at: ZERO-LENGTH write, buf.len()={}, _len={}",
        //         buf.len(),
        //         _len
        //     );
        // }
        drop(data);
        self.write(buf).map_err(Into::into)
    }

    fn write_user_buffer_at(
        &self,
        _offset: usize,
        len: usize,
        source: &crate::syscall::user_buffer::UserBuffer<'_>,
        data: MutexGuard<FilePrivateData>,
    ) -> Result<Option<usize>, crate::mm::allocation::MemoryAllocationError> {
        drop(data);
        let Some(address) = source.as_contiguous_address() else {
            // A vector retains the existing single-record kernel fallback.
            return Ok(None);
        };
        let reader = UserBufferReader::new(address.data() as *const u8, source.len(), true)?;
        self.send_user_buffer(&reader, len, super::PMSG::empty(), None)
            .map(Some)
            .map_err(Into::into)
    }

    fn resize(&self, _len: usize) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
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
        super::base::Socket::mmap_fs(self).unwrap_or_else(super::fs::SockFs::instance)
    }

    /// Sockets expose a `SockFs` identity for `fstatfs`, but they are not
    /// attached to a mount. Keeping `try_fs()` empty preserves the VFS
    /// fast path that skips mount-backed atime handling for socket I/O.
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

    fn metadata(&self) -> Result<Metadata, SystemError> {
        Ok(self.inode_metadata().snapshot(self.socket_inode_id()))
    }

    fn set_metadata_masked(
        &self,
        metadata: &Metadata,
        mask: SetMetadataMask,
    ) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
        self.inode_metadata().set_metadata_masked(metadata, mask);
        Ok(())
    }

    fn update_metadata_masked(
        &self,
        update: &mut MetadataUpdate<'_>,
    ) -> Result<SetMetadataMask, crate::mm::allocation::MemoryAllocationError> {
        self.inode_metadata()
            .update_metadata_masked(update)
            .map_err(Into::into)
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
    ) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
        match cmd {
            SIOCGIFCONF => handle_siocgifconf(self.netns(), data).map_err(Into::into),
            SIOCGIFINDEX | SIOCGIFFLAGS | SIOCGIFMTU | SIOCGIFHWADDR | SIOCGIFTXQLEN => {
                handle_netdev_query(self.netns(), cmd, data).map_err(Into::into)
            }
            SIOCSIFFLAGS | SIOCSIFMTU => handle_netdev_mutation(self.netns(), cmd, data).map_err(Into::into),
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
                Socket::ioctl(self, cmd, data, &private_data).map_err(Into::into)
            }
        }
    }

    fn as_socket(&self) -> Option<&dyn Socket> {
        Some(self)
    }
}

impl<T: Socket + 'static> PollableInode for T {
    fn set_owner_file(&self, file: alloc::sync::Weak<File>) {
        Socket::set_owner_file(self, file);
    }

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
