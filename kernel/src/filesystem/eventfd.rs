use super::vfs::PollableInode;
use crate::filesystem::anon_inode::{anon_inode_metadata, anon_inode_path, AnonInodeFs};
use crate::filesystem::epoll::event_poll::EPollItemList;
use crate::filesystem::vfs::file::FileFlags;
use crate::filesystem::vfs::{InodeMode, OpenFileBehavior, PostWriteSyncPolicy, UserReadMode};
use crate::filesystem::{
    epoll::{event_poll::EventPoll, EPollEventType, EPollItem},
    vfs::{FilePrivateData, FileSystem, IndexNode, Metadata},
};
use crate::libs::mutex::{Mutex, MutexGuard};
use crate::libs::wait_queue::WaitQueue;
use crate::process::ProcessManager;
use crate::syscall::user_buffer::UserBuffer;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use ida::IdAllocator;
use system_error::SystemError;

const EVENTFD_MAX: u64 = u64::MAX - 1;

pub static EVENTFD_ID_ALLOCATOR: Mutex<IdAllocator> =
    Mutex::new(IdAllocator::new(0, u32::MAX as usize).unwrap());

bitflags! {
    pub struct EventFdFlags: u32{
        /// Provide semaphore-like semantics for reads from the new
        /// file descriptor.
        const EFD_SEMAPHORE = 0o1;
        /// Set the close-on-exec (FD_CLOEXEC) flag on the new file
        /// descriptor
        const EFD_CLOEXEC = 0o2000000;
        /// Set the O_NONBLOCK file status flag on the open file
        /// description (see open(2)) referred to by the new file
        /// descriptor
        const EFD_NONBLOCK = 0o0004000;
    }
}

#[derive(Debug)]
pub struct EventFd {
    count: u64,
    flags: EventFdFlags,
    #[allow(unused)]
    id: u32,
}

impl EventFd {
    pub fn new(count: u64, flags: EventFdFlags, id: u32) -> Self {
        EventFd { count, flags, id }
    }
}

#[derive(Debug)]
pub struct EventFdInode {
    eventfd: Mutex<EventFd>,
    wait_queue: WaitQueue,
    epitems: EPollItemList,
}

impl EventFdInode {
    pub fn new(eventfd: EventFd) -> Self {
        EventFdInode {
            eventfd: Mutex::new(eventfd),
            wait_queue: WaitQueue::default(),
            epitems: EPollItemList::default(),
        }
    }
    /// Consume the counter and publish writer readiness before userspace copying.
    /// A later copy fault does not restore the counter, matching eventfd_read().
    fn read_counter(&self, data: &FilePrivateData) -> Result<u64, SystemError> {
        let mut lock_efd = self.eventfd.lock();
        while lock_efd.count == 0 {
            if lock_efd.flags.contains(EventFdFlags::EFD_NONBLOCK) {
                drop(lock_efd);
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }

            drop(lock_efd);

            if ProcessManager::current_pcb().has_pending_signal_fast() {
                return Err(SystemError::ERESTARTSYS);
            }

            wq_wait_event_interruptible!(self.wait_queue, self.readable(), {})?;

            lock_efd = self.eventfd.lock();
        }
        let mut val = lock_efd.count;

        let mut eventfd = lock_efd;
        if eventfd.flags.contains(EventFdFlags::EFD_SEMAPHORE) {
            eventfd.count -= 1;
            val = 1;
        } else {
            eventfd.count = 0;
        }
        let pollflag = EPollEventType::from_bits_truncate(self.do_poll(data, &eventfd)? as u32);
        drop(eventfd);

        self.wait_queue.wakeup_all(None);
        // 唤醒epoll中等待的进程
        EventPoll::wakeup_epoll(&self.epitems, pollflag)?;

        Ok(val)
    }

    /// Counter admission, waiting and readiness publication shared by kernel
    /// and protected userspace writes. No File private-data guard is retained.
    fn write_counter(&self, val: u64, data: &FilePrivateData) -> Result<usize, SystemError> {
        if val == u64::MAX {
            return Err(SystemError::EINVAL);
        }
        let pollflag;
        loop {
            let mut eventfd = self.eventfd.lock();
            // Allow write when count + val <= EVENTFD_MAX.
            if EVENTFD_MAX.saturating_sub(eventfd.count) >= val {
                eventfd.count += val;
                pollflag = EPollEventType::from_bits_truncate(self.do_poll(data, &eventfd)? as u32);
                break;
            }
            if eventfd.flags.contains(EventFdFlags::EFD_NONBLOCK) {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
            drop(eventfd);
            if ProcessManager::current_pcb().has_pending_signal() {
                return Err(SystemError::ERESTARTSYS);
            }
            // 等待计数下降（被 read 消费）
            self.wait_queue.wait_event_interruptible(
                || {
                    let ev = self.eventfd.lock();
                    EVENTFD_MAX.saturating_sub(ev.count) >= val
                },
                None::<fn()>,
            )?;
        }
        self.wait_queue.wakeup_all(None);

        // 唤醒epoll中等待的进程
        EventPoll::wakeup_epoll(&self.epitems, pollflag)?;
        return Ok(8);
    }

    fn readable(&self) -> bool {
        let count = self.eventfd.lock().count;
        return count > 0;
    }

    fn do_poll(
        &self,
        _private_data: &FilePrivateData,
        self_guard: &MutexGuard<'_, EventFd>,
    ) -> Result<usize, SystemError> {
        let mut events = EPollEventType::empty();
        if self_guard.count != 0 {
            events |= EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM;
        }
        // Linux eventfd counter max is U64_MAX - 1. When count reaches that max,
        // any further write(8) would block (or EAGAIN for nonblock), so EPOLLOUT
        // must be cleared.
        if self_guard.count != EVENTFD_MAX {
            events |= EPollEventType::EPOLLOUT | EPollEventType::EPOLLWRNORM;
        }
        return Ok(events.bits() as usize);
    }
}

impl PollableInode for EventFdInode {
    fn poll(&self, _private_data: &FilePrivateData) -> Result<usize, SystemError> {
        let self_guard = self.eventfd.lock();
        self.do_poll(_private_data, &self_guard)
    }

    fn add_epitem(
        &self,
        epitem: Arc<EPollItem>,
        _private_data: &FilePrivateData,
    ) -> Result<(), SystemError> {
        self.epitems.add(epitem);
        Ok(())
    }

    fn remove_epitem(
        &self,
        epitem: &Arc<EPollItem>,
        _private_data: &FilePrivateData,
    ) -> Result<(), SystemError> {
        self.epitems.remove(epitem)
    }
}

impl IndexNode for EventFdInode {
    fn supports_write_iter_flags(&self) -> bool {
        false
    }

    fn configure_open_file(&self, _data: &FilePrivateData, behavior: &mut OpenFileBehavior) {
        behavior.post_write_sync = PostWriteSyncPolicy::NotApplicable;
    }

    fn is_stream(&self) -> bool {
        // eventfd 属于典型的不可 seek 的伪文件描述符：
        // - lseek/pread/pwrite 应返回 ESPIPE（Linux 语义）
        true
    }

    fn open(
        &self,
        _data: MutexGuard<FilePrivateData>,
        _flags: &FileFlags,
    ) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
        let result: core::result::Result<_, system_error::SystemError> = Ok(());
        result.map_err(Into::into)
    }

    fn close(&self, _data: MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        Ok(())
    }

    /// # 从 counter 里读取一个 8 字节的int值
    ///
    /// 1. counter !=0
    ///     - EFD_SEMAPHORE 如果没有被设置，从 eventfd read，会得到 counter，并将它归0
    ///     - EFD_SEMAPHORE 如果被设置，从 eventfd read，会得到值 1，并将 counter - 1
    /// 2. counter == 0
    ///     - EFD_NONBLOCK 如果被设置，那么会以 EAGAIN 的错失败
    ///     - 否则 read 会被阻塞，直到为非0。
    fn read_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &mut [u8],
        data_guard: MutexGuard<FilePrivateData>,
    ) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
        let data = data_guard.clone();
        drop(data_guard);
        if len < 8 || buf.len() < 8 {
            return Err(SystemError::EINVAL.into());
        }
        let value = self.read_counter(&data)?;
        buf[..8].copy_from_slice(&value.to_ne_bytes());
        Ok(8)
    }

    fn user_read_mode(&self) -> UserReadMode {
        UserReadMode::Iterator
    }

    fn read_user_at(
        &self,
        _offset: usize,
        len: usize,
        writer: &mut UserBuffer<'_>,
        data_guard: MutexGuard<FilePrivateData>,
    ) -> Result<Option<usize>, SystemError> {
        let data = data_guard.clone();
        drop(data_guard);
        if len < 8 {
            return Err(SystemError::EINVAL);
        }
        let value = self.read_counter(&data)?;
        writer.write_one(0, &value)?;
        Ok(Some(8))
    }

    /// # 把一个 8 字节的int值写入到 counter 里
    ///
    /// - counter 最大值是 2^64 - 1
    /// - 如果写入时会发生溢出，则write会被阻塞
    ///     - 如果 EFD_NONBLOCK 被设置，那么以 EAGAIN 失败
    /// - 以不合法的值写入时，会以 EINVAL 失败
    ///     - 比如 0xffffffffffffffff 不合法
    ///     -  比如 写入的值 size 小于8字节
    fn write_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &[u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
        let snapshot = data.clone();
        drop(data);
        if len < 8 || buf.len() < 8 {
            return Err(SystemError::EINVAL.into());
        }
        let value = u64::from_ne_bytes(buf[..8].try_into().unwrap());
        self.write_counter(value, &snapshot).map_err(Into::into)
    }

    fn write_user_buffer_at(
        &self,
        _offset: usize,
        len: usize,
        source: &UserBuffer<'_>,
        data: MutexGuard<FilePrivateData>,
    ) -> Result<Option<usize>, crate::mm::allocation::MemoryAllocationError> {
        let snapshot = data.clone();
        drop(data);
        // eventfd_write() checks count before copying exactly one record.
        if len < 8 {
            return Err(SystemError::EINVAL.into());
        }
        let mut bytes = [0; 8];
        source.read_from_user(0, &mut bytes)?;
        self.write_counter(u64::from_ne_bytes(bytes), &snapshot)
            .map(Some)
            .map_err(Into::into)
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        Ok(anon_inode_metadata(InodeMode::S_IRUSR | InodeMode::S_IWUSR))
    }

    fn stat_mode(&self, metadata: &Metadata) -> InodeMode {
        metadata.mode
    }

    fn resize(&self, _len: usize) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
        let result: core::result::Result<_, system_error::SystemError> = Ok(());
        result.map_err(Into::into)
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        AnonInodeFs::instance()
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn list(&self) -> Result<Vec<String>, SystemError> {
        Err(SystemError::EINVAL)
    }

    fn as_pollable_inode(&self) -> Result<&dyn PollableInode, SystemError> {
        Ok(self)
    }

    fn absolute_path(&self) -> Result<String, SystemError> {
        Ok(anon_inode_path("[eventfd]"))
    }
}
