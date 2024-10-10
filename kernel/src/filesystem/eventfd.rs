use crate::filesystem::vfs::file::{File, FileMode};
use crate::filesystem::vfs::syscall::ModeType;
use crate::filesystem::vfs::{FilePrivateData, FileSystem, FileType, IndexNode, Metadata};
use crate::libs::spinlock::{SpinLock, SpinLockGuard};
use crate::libs::wait_queue::WaitQueue;
use crate::net::event_poll::{EPollEventType, EPollItem, EventPoll, KernelIoctlData};
use crate::process::ProcessManager;
use crate::syscall::Syscall;
use alloc::collections::LinkedList;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::sync::Weak;
use alloc::vec::Vec;
use core::any::Any;
use ida::IdAllocator;
use system_error::SystemError;

static EVENTFD_ID_ALLOCATOR: IdAllocator = IdAllocator::new(0, u32::MAX as usize);

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
    pub fn remove_epoll(&self, epoll: &Weak<SpinLock<EventPoll>>) -> Result<(), SystemError> {
        let is_remove = !self
            .epitems
            .lock_irqsave()
            .extract_if(|x| x.epoll().ptr_eq(epoll))
            .collect::<Vec<_>>()
            .is_empty();

        if is_remove {
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
        data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        if len < 8 {
            return Err(SystemError::EINVAL);
        }
        let mut val = loop {
            let val = self.eventfd.lock().count;
            if val != 0 {
                break val;
            }
            if self
                .eventfd
                .lock()
                .flags
                .contains(EventFdFlags::EFD_NONBLOCK)
            {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
            self.wait_queue.sleep();
        };

        let mut eventfd = self.eventfd.lock();
        if eventfd.flags.contains(EventFdFlags::EFD_SEMAPHORE) {
            eventfd.count -= 1;
            val = 1;
        } else {
            eventfd.count = 0;
        }
        let val_bytes = val.to_ne_bytes();
        buf[..8].copy_from_slice(&val_bytes);

        let pollflag = EPollEventType::from_bits_truncate(self.poll(&data)? as u32);
        // 唤醒epoll中等待的进程
        EventPoll::wakeup_epoll(&self.epitems, pollflag)?;

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
            self.wait_queue.sleep();
        }
        let mut eventfd = self.eventfd.lock();
        eventfd.count += val;
        self.wait_queue.wakeup_all(None);

        let pollflag = EPollEventType::from_bits_truncate(self.poll(&data)? as u32);
        // 唤醒epoll中等待的进程
        EventPoll::wakeup_epoll(&self.epitems, pollflag)?;
        return Ok(8);
    }

    /// # 检查 eventfd 的状态
    ///
    /// - 如果 counter 的值大于 0 ，那么 fd 的状态就是可读的
    /// - 如果能无阻塞地写入一个至少为 1 的值，那么 fd 的状态就是可写的
    fn poll(&self, _private_data: &FilePrivateData) -> Result<usize, SystemError> {
        let mut events = EPollEventType::empty();
        if self.eventfd.lock().count != 0 {
            events |= EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM;
        }
        if self.eventfd.lock().count != u64::MAX {
            events |= EPollEventType::EPOLLOUT | EPollEventType::EPOLLWRNORM;
        }
        return Ok(events.bits() as usize);
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
    fn kernel_ioctl(
        &self,
        arg: Arc<dyn KernelIoctlData>,
        _data: &FilePrivateData,
    ) -> Result<usize, SystemError> {
        let epitem = arg
            .arc_any()
            .downcast::<EPollItem>()
            .map_err(|_| SystemError::EFAULT)?;
        self.epitems.lock().push_back(epitem);
        Ok(0)
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
        let id = EVENTFD_ID_ALLOCATOR.alloc().ok_or(SystemError::ENOMEM)? as u32;
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
