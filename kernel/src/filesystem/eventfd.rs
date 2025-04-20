use super::vfs::PollableInode;
use crate::filesystem::vfs::file::{File, FileMode};
use crate::filesystem::vfs::syscall::ModeType;
use crate::filesystem::vfs::{FilePrivateData, FileSystem, FileType, IndexNode, Metadata};
use crate::libs::spinlock::{SpinLock, SpinLockGuard};
use crate::libs::wait_queue::WaitQueue;
use crate::net::event_poll::{EPollEventType, EPollItem, EventPoll};
use crate::process::{ProcessFlags, ProcessManager};
use crate::sched::SchedMode;
use crate::syscall::Syscall;
use alloc::collections::LinkedList;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use ida::IdAllocator;
use system_error::SystemError;

static EVENTFD_ID_ALLOCATOR: SpinLock<IdAllocator> =
    SpinLock::new(IdAllocator::new(0, u32::MAX as usize).unwrap());

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
    eventfd: SpinLock<EventFd>,
    wait_queue: WaitQueue,
    epitems: SpinLock<LinkedList<Arc<EPollItem>>>,
}

impl EventFdInode {
    pub fn new(eventfd: EventFd) -> Self {
        EventFdInode {
            eventfd: SpinLock::new(eventfd),
            wait_queue: WaitQueue::default(),
            epitems: SpinLock::new(LinkedList::new()),
        }
    }
    fn readable(&self) -> bool {
        let count = self.eventfd.lock().count;
        return count > 0;
    }

    fn do_poll(
        &self,
        _private_data: &FilePrivateData,
        self_guard: &SpinLockGuard<'_, EventFd>,
    ) -> Result<usize, SystemError> {
        let mut events = EPollEventType::empty();
        if self_guard.count != 0 {
            events |= EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM;
        }
        if self_guard.count != u64::MAX {
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
        self.epitems.lock().push_back(epitem);
        Ok(())
    }

    fn remove_epitem(
        &self,
        epitem: &Arc<EPollItem>,
        _private_data: &FilePrivateData,
    ) -> Result<(), SystemError> {
        let mut guard = self.epitems.lock();
        let len = guard.len();
        guard.retain(|x| !Arc::ptr_eq(x, epitem));
        if len != guard.len() {
            return Ok(());
        }
        Err(SystemError::ENOENT)
    }
}

impl IndexNode for EventFdInode {
    fn open(
        &self,
        _data: SpinLockGuard<FilePrivateData>,
        _mode: &FileMode,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    fn close(&self, _data: SpinLockGuard<FilePrivateData>) -> Result<(), SystemError> {
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
        data_guard: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let data = data_guard.clone();
        drop(data_guard);
        if len < 8 {
            return Err(SystemError::EINVAL);
        }
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

            let r = wq_wait_event_interruptible!(self.wait_queue, self.readable(), {});
            if r.is_err() {
                ProcessManager::current_pcb()
                    .flags()
                    .insert(ProcessFlags::HAS_PENDING_SIGNAL);

                return Err(SystemError::ERESTARTSYS);
            }

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
        let val_bytes = val.to_ne_bytes();
        buf[..8].copy_from_slice(&val_bytes);
        let pollflag = EPollEventType::from_bits_truncate(self.do_poll(&data, &eventfd)? as u32);
        drop(eventfd);

        // 唤醒epoll中等待的进程
        EventPoll::wakeup_epoll(&self.epitems, Some(pollflag))?;

        return Ok(8);
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
        data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        if len < 8 {
            return Err(SystemError::EINVAL);
        }
        let val = u64::from_ne_bytes(buf[..8].try_into().unwrap());
        if val == u64::MAX {
            return Err(SystemError::EINVAL);
        }
        loop {
            if ProcessManager::current_pcb().has_pending_signal() {
                return Err(SystemError::ERESTARTSYS);
            }
            let eventfd = self.eventfd.lock();
            if u64::MAX - eventfd.count > val {
                break;
            }
            // block until a read() is performed  on the
            // file descriptor, or fails with the error EAGAIN if the
            // file descriptor has been made nonblocking.
            if eventfd.flags.contains(EventFdFlags::EFD_NONBLOCK) {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
            drop(eventfd);
            self.wait_queue.sleep().ok();
        }
        let mut eventfd = self.eventfd.lock();
        eventfd.count += val;
        drop(eventfd);
        self.wait_queue.wakeup_all(None);

        let eventfd = self.eventfd.lock();
        let pollflag = EPollEventType::from_bits_truncate(self.do_poll(&data, &eventfd)? as u32);
        drop(eventfd);

        // 唤醒epoll中等待的进程
        EventPoll::wakeup_epoll(&self.epitems, Some(pollflag))?;
        return Ok(8);
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        let meta = Metadata {
            mode: ModeType::from_bits_truncate(0o755),
            file_type: FileType::File,
            ..Default::default()
        };
        Ok(meta)
    }

    fn resize(&self, _len: usize) -> Result<(), SystemError> {
        Ok(())
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        panic!("EventFd does not have a filesystem")
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
}

impl Syscall {
    /// # 创建一个 eventfd 文件描述符
    ///
    /// ## 参数
    /// - `init_val`: u32: eventfd 的初始值
    /// - `flags`: u32: eventfd 的标志
    ///
    /// ## 返回值
    /// - `Ok(usize)`: 成功创建的文件描述符
    /// - `Err(SystemError)`: 创建失败
    ///
    /// See: https://man7.org/linux/man-pages/man2/eventfd2.2.html
    pub fn sys_eventfd(init_val: u32, flags: u32) -> Result<usize, SystemError> {
        let flags = EventFdFlags::from_bits(flags).ok_or(SystemError::EINVAL)?;
        let id = EVENTFD_ID_ALLOCATOR
            .lock()
            .alloc()
            .ok_or(SystemError::ENOMEM)? as u32;
        let eventfd = EventFd::new(init_val as u64, flags, id);
        let inode = Arc::new(EventFdInode::new(eventfd));
        let filemode = if flags.contains(EventFdFlags::EFD_CLOEXEC) {
            FileMode::O_RDWR | FileMode::O_CLOEXEC
        } else {
            FileMode::O_RDWR
        };
        let file = File::new(inode, filemode)?;
        let binding = ProcessManager::current_pcb().fd_table();
        let mut fd_table_guard = binding.write();
        let fd = fd_table_guard.alloc_fd(file, None).map(|x| x as usize);
        return fd;
    }
}
