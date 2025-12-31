use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::arch::MMArch;
use crate::filesystem::epoll::event_poll::LockedEPItemLinkedList;
use crate::filesystem::epoll::{event_poll::EventPoll, EPollEventType, EPollItem};
use crate::filesystem::vfs::file::FileFlags;
use crate::filesystem::vfs::{
    FilePrivateData, FileSystem, FileType, FsInfo, IndexNode, InodeMode, Magic, Metadata,
    PollableInode, SuperBlock,
};
use crate::libs::spinlock::{SpinLock, SpinLockGuard};
use crate::libs::wait_queue::WaitQueue;
use crate::mm::MemoryManagementArch;
use crate::process::{ProcessFlags, ProcessManager};
use system_error::SystemError;

use super::registry::{register_instance, unregister_instance, InodeKey};
use super::uapi::{align4, InotifyCookie, InotifyEvent, InotifyMask, WatchDescriptor};

const FIONREAD: u32 = 0x541B;

const DEFAULT_MAX_QUEUED_EVENTS: usize = 16384;

lazy_static::lazy_static! {
    static ref INOTIFY_FS: Arc<InotifyFs> = Arc::new(InotifyFs);
}

#[derive(Debug)]
pub struct InotifyFs;

impl InotifyFs {
    pub fn instance() -> Arc<InotifyFs> {
        INOTIFY_FS.clone()
    }
}

impl FileSystem for InotifyFs {
    fn root_inode(&self) -> Arc<dyn IndexNode> {
        Arc::new(InotifyInode::new(false))
    }

    fn info(&self) -> FsInfo {
        FsInfo {
            blk_dev_id: 0,
            max_name_len: 255,
        }
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        "inotify"
    }

    fn super_block(&self) -> SuperBlock {
        SuperBlock::new(Magic::EVENTFD_MAGIC, MMArch::PAGE_SIZE as u64, 255)
    }
}

#[derive(Debug, Clone)]
pub struct QueuedEvent {
    pub wd: WatchDescriptor,
    pub mask: InotifyMask,
    pub cookie: InotifyCookie,
    pub name: Option<Vec<u8>>, // includes trailing NUL
}

impl QueuedEvent {
    pub fn encoded_len(&self) -> usize {
        let name_len = self.name.as_ref().map(|v| v.len()).unwrap_or(0);
        InotifyEvent::SIZE + align4(name_len)
    }

    pub fn write_into(&self, buf: &mut [u8]) -> usize {
        let name_len = self.name.as_ref().map(|v| v.len()).unwrap_or(0);
        // pad to 4-byte boundary
        let padded = align4(name_len);

        let event = InotifyEvent {
            wd: self.wd,
            mask: self.mask,
            cookie: self.cookie,
            len: padded as u32,
        };

        let header = unsafe {
            core::slice::from_raw_parts(
                (&event as *const InotifyEvent) as *const u8,
                InotifyEvent::SIZE,
            )
        };
        buf[..InotifyEvent::SIZE].copy_from_slice(header);

        if let Some(name) = self.name.as_ref() {
            buf[InotifyEvent::SIZE..InotifyEvent::SIZE + name.len()].copy_from_slice(name);
        }
        // pad to 4-byte boundary
        let padded = align4(name_len);
        for b in &mut buf[InotifyEvent::SIZE + name_len..InotifyEvent::SIZE + padded] {
            *b = 0;
        }
        InotifyEvent::SIZE + padded
    }
}

#[derive(Debug)]
struct InotifyState {
    nonblock: bool,

    events: VecDeque<QueuedEvent>,
    bytes_available: usize,

    overflowed: bool,

    max_queued_events: usize,
}

impl InotifyState {
    fn new(nonblock: bool) -> Self {
        Self {
            nonblock,
            events: VecDeque::new(),
            bytes_available: 0,
            overflowed: false,
            max_queued_events: DEFAULT_MAX_QUEUED_EVENTS,
        }
    }

    fn readable(&self) -> bool {
        !self.events.is_empty()
    }

    fn push_event(&mut self, ev: QueuedEvent) {
        // Linux 语义：对“重复的相同事件”进行合并（coalesce），避免读者被大量重复事件淹没。
        // 这里实现最小可用：若队尾事件与新事件在 wd/mask/cookie/name 上完全一致，则丢弃新事件。
        if let Some(last) = self.events.back() {
            if last.wd == ev.wd
                && last.mask == ev.mask
                && last.cookie == ev.cookie
                && last.name == ev.name
            {
                return;
            }
        }

        if self.events.len() >= self.max_queued_events {
            if !self.overflowed {
                self.overflowed = true;
                let overflow = QueuedEvent {
                    wd: WatchDescriptor(-1),
                    mask: InotifyMask::IN_Q_OVERFLOW,
                    cookie: InotifyCookie(0),
                    name: None,
                };
                self.bytes_available += overflow.encoded_len();
                self.events.push_back(overflow);
            }
            return;
        }
        self.bytes_available += ev.encoded_len();
        self.events.push_back(ev);
    }

    fn pop_into(&mut self, buf: &mut [u8]) -> Result<usize, SystemError> {
        if self.events.is_empty() {
            return Ok(0);
        }

        let first_len = self.events.front().unwrap().encoded_len();
        if buf.len() < first_len {
            return Err(SystemError::EINVAL);
        }

        let mut written = 0usize;
        while let Some(ev) = self.events.front() {
            let need = ev.encoded_len();
            if written + need > buf.len() {
                break;
            }
            let ev = self.events.pop_front().unwrap();
            let n = ev.write_into(&mut buf[written..written + need]);
            written += n;
            self.bytes_available = self.bytes_available.saturating_sub(need);
        }

        if self.events.is_empty() {
            self.overflowed = false;
        }

        Ok(written)
    }
}

/// Inotify instance pseudo inode.
#[derive(Debug)]
pub struct InotifyInode {
    state: SpinLock<InotifyState>,
    wait_queue: WaitQueue,
    epitems: LockedEPItemLinkedList,

    destroyed: AtomicBool,
    instance_id: u32,
}

static INSTANCE_ID: AtomicU32 = AtomicU32::new(1);

impl InotifyInode {
    pub fn new(nonblock: bool) -> Self {
        let instance_id = INSTANCE_ID.fetch_add(1, Ordering::Relaxed);
        let this = Self {
            state: SpinLock::new(InotifyState::new(nonblock)),
            wait_queue: WaitQueue::default(),
            epitems: LockedEPItemLinkedList::default(),
            destroyed: AtomicBool::new(false),
            instance_id,
        };
        register_instance(this.instance_id);
        this
    }

    pub fn instance_id(&self) -> u32 {
        self.instance_id
    }

    fn do_poll(
        &self,
        _private_data: &FilePrivateData,
        guard: &SpinLockGuard<'_, InotifyState>,
    ) -> Result<usize, SystemError> {
        let mut events = EPollEventType::empty();
        if guard.readable() {
            events |= EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM;
        }
        // always writable (inotify is read-only, but Linux poll reports EPOLLOUT for regular fds).
        events |= EPollEventType::EPOLLOUT | EPollEventType::EPOLLWRNORM;
        Ok(events.bits() as usize)
    }

    pub fn enqueue_event(&self, ev: QueuedEvent) -> Result<(), SystemError> {
        if self.destroyed.load(Ordering::Relaxed) {
            return Ok(());
        }

        let mut guard = self.state.lock();
        guard.push_event(ev);
        let pollflag = EPollEventType::from_bits_truncate(
            self.do_poll(&FilePrivateData::default(), &guard)? as u32,
        );
        drop(guard);

        self.wait_queue.wakeup_all(None);
        EventPoll::wakeup_epoll(&self.epitems, pollflag)?;
        Ok(())
    }

    pub fn bytes_available(&self) -> usize {
        self.state.lock().bytes_available
    }

    #[allow(dead_code)]
    pub fn nonblock(&self) -> bool {
        self.state.lock().nonblock
    }

    #[allow(dead_code)]
    pub fn inode_key(&self) -> InodeKey {
        // inotify instance itself is not a watched inode; this is only used for debugging.
        InodeKey {
            dev_id: 0,
            inode_id: self.instance_id as usize,
        }
    }

    fn readable(&self) -> bool {
        self.state.lock().readable()
    }
}

impl PollableInode for InotifyInode {
    fn poll(&self, private_data: &FilePrivateData) -> Result<usize, SystemError> {
        let guard = self.state.lock();
        self.do_poll(private_data, &guard)
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

impl IndexNode for InotifyInode {
    fn is_stream(&self) -> bool {
        true
    }

    fn open(
        &self,
        _data: SpinLockGuard<FilePrivateData>,
        _flags: &FileFlags,
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
        data_guard: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        // 关键：File::do_read 会持有 file.private_data 的 SpinLockGuard 传入 read_at。
        // inotify 的 read 可能阻塞等待事件，若持锁睡眠会导致 preempt_count != 0，进而在 schedule()
        // 处触发断言 panic。因此必须在任何可能睡眠之前显式释放该 guard。
        drop(data_guard);

        if len == 0 {
            return Ok(0);
        }

        // Linux 语义：若 read 的 count 小于 inotify_event 头部长度，直接返回 EINVAL。
        if len < InotifyEvent::SIZE {
            return Err(SystemError::EINVAL);
        }

        loop {
            {
                let mut guard = self.state.lock();
                if guard.readable() {
                    let n = guard.pop_into(&mut buf[..len])?;
                    let pollflag = EPollEventType::from_bits_truncate(
                        self.do_poll(&FilePrivateData::default(), &guard)? as u32,
                    );
                    drop(guard);
                    EventPoll::wakeup_epoll(&self.epitems, pollflag)?;
                    return Ok(n);
                }

                if guard.nonblock {
                    return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                }
            }

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
        }
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        Err(SystemError::EBADF)
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        Ok(Metadata {
            mode: InodeMode::from_bits_truncate(0o600),
            file_type: FileType::File,
            ..Default::default()
        })
    }

    fn resize(&self, _len: usize) -> Result<(), SystemError> {
        Ok(())
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        InotifyFs::instance()
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn as_pollable_inode(&self) -> Result<&dyn PollableInode, SystemError> {
        Ok(self)
    }

    fn absolute_path(&self) -> Result<String, SystemError> {
        Ok(String::from("inotify"))
    }

    fn ioctl(
        &self,
        cmd: u32,
        data: usize,
        _private_data: &FilePrivateData,
    ) -> Result<usize, SystemError> {
        match cmd {
            FIONREAD => {
                let available = self.bytes_available() as i32;
                let mut writer = crate::syscall::user_access::UserBufferWriter::new(
                    data as *mut u8,
                    core::mem::size_of::<i32>(),
                    true,
                )?;
                writer
                    .buffer_protected(0)?
                    .write_one::<i32>(0, &available)?;
                Ok(0)
            }
            _ => Err(SystemError::ENOSYS),
        }
    }
}

impl Drop for InotifyInode {
    fn drop(&mut self) {
        self.destroyed.store(true, Ordering::Relaxed);
        unregister_instance(self.instance_id);
    }
}
