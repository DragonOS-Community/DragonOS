use crate::filesystem::vfs::file::{File, FileMode};
use crate::filesystem::vfs::syscall::ModeType;
use crate::filesystem::vfs::{FilePrivateData, FileSystem, FileType, IndexNode, Metadata};
use crate::libs::spinlock::{SpinLock, SpinLockGuard};
use crate::libs::wait_queue::WaitQueue;
use crate::net::event_poll::EPollEventType;
use crate::process::ProcessManager;
use crate::syscall::Syscall;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::AtomicU32;
use system_error::SystemError;

static EVENTFD_ID: AtomicU32 = AtomicU32::new(0);

bitflags! {
    pub struct EventFdFlags: u32{
        const EFD_SEMAPHORE = 1;
        const EFD_CLOEXEC = 2;
        const EFD_NONBLOCK = 4;
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
}

impl EventFdInode {
    pub fn new(eventfd: EventFd) -> Self {
        EventFdInode {
            eventfd: SpinLock::new(eventfd),
            wait_queue: WaitQueue::default(),
        }
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

    fn read_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: SpinLockGuard<FilePrivateData>,
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
        return Ok(8);
    }

    fn write_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &[u8],
        _data: SpinLockGuard<FilePrivateData>,
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
        return Ok(8);
    }

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
    pub fn sys_eventfd(init_val: u32, flags: u32) -> Result<usize, SystemError> {
        let flags = EventFdFlags::from_bits(flags).ok_or(SystemError::EINVAL)?;
        let id = EVENTFD_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
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
