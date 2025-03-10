use core::sync::atomic::compiler_fence;

use crate::{
    arch::ipc::signal::{SigCode, Signal},
    filesystem::vfs::{
        core::generate_inode_id, file::FileMode, syscall::ModeType, FilePrivateData, FileSystem,
        FileType, IndexNode, Metadata,
    },
    libs::{
        spinlock::{SpinLock, SpinLockGuard},
        wait_queue::WaitQueue,
    },
    net::event_poll::{EPollEventType, EPollItem, EventPoll},
    process::{ProcessFlags, ProcessManager, ProcessState},
    sched::SchedMode,
    time::PosixTimeSpec,
};

use alloc::{
    collections::LinkedList,
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

use super::signal_types::{SigInfo, SigType};

/// 我们设定pipe_buff的总大小为1024字节
const PIPE_BUFF_SIZE: usize = 1024;

#[derive(Debug, Clone)]
pub struct PipeFsPrivateData {
    mode: FileMode,
}

impl PipeFsPrivateData {
    pub fn new(mode: FileMode) -> Self {
        return PipeFsPrivateData { mode };
    }

    pub fn set_mode(&mut self, mode: FileMode) {
        self.mode = mode;
    }
}

/// @brief 管道文件i节点(锁)
#[derive(Debug)]
pub struct LockedPipeInode {
    inner: SpinLock<InnerPipeInode>,
    read_wait_queue: WaitQueue,
    write_wait_queue: WaitQueue,
    epitems: SpinLock<LinkedList<Arc<EPollItem>>>,
}

/// @brief 管道文件i节点(无锁)
#[derive(Debug)]
pub struct InnerPipeInode {
    self_ref: Weak<LockedPipeInode>,
    /// 管道内可读的数据数
    valid_cnt: i32,
    read_pos: i32,
    write_pos: i32,
    data: [u8; PIPE_BUFF_SIZE],
    /// INode 元数据
    metadata: Metadata,
    reader: u32,
    writer: u32,
    had_reader: bool,
}

impl InnerPipeInode {
    pub fn poll(&self, private_data: &FilePrivateData) -> Result<usize, SystemError> {
        let mut events = EPollEventType::empty();

        let mode = if let FilePrivateData::Pipefs(PipeFsPrivateData { mode }) = private_data {
            mode
        } else {
            return Err(SystemError::EBADFD);
        };

        if mode.contains(FileMode::O_RDONLY) {
            if self.valid_cnt != 0 {
                // 有数据可读
                events.insert(EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM);
            }

            // 没有写者
            if self.writer == 0 {
                events.insert(EPollEventType::EPOLLHUP)
            }
        }

        if mode.contains(FileMode::O_WRONLY) {
            // 管道内数据未满
            if self.valid_cnt as usize != PIPE_BUFF_SIZE {
                events.insert(EPollEventType::EPOLLOUT | EPollEventType::EPOLLWRNORM);
            }

            // 没有读者
            if self.reader == 0 {
                events.insert(EPollEventType::EPOLLERR);
            }
        }

        Ok(events.bits() as usize)
    }

    fn buf_full(&self) -> bool {
        return self.valid_cnt as usize == PIPE_BUFF_SIZE;
    }
}

impl LockedPipeInode {
    pub fn new() -> Arc<Self> {
        let inner = InnerPipeInode {
            self_ref: Weak::default(),
            valid_cnt: 0,
            read_pos: 0,
            write_pos: 0,
            had_reader: false,
            data: [0; PIPE_BUFF_SIZE],

            metadata: Metadata {
                dev_id: 0,
                inode_id: generate_inode_id(),
                size: PIPE_BUFF_SIZE as i64,
                blk_size: 0,
                blocks: 0,
                atime: PosixTimeSpec::default(),
                mtime: PosixTimeSpec::default(),
                ctime: PosixTimeSpec::default(),
                file_type: FileType::Pipe,
                mode: ModeType::from_bits_truncate(0o666),
                nlinks: 1,
                uid: 0,
                gid: 0,
                raw_dev: Default::default(),
            },
            reader: 0,
            writer: 0,
        };
        let result = Arc::new(Self {
            inner: SpinLock::new(inner),
            read_wait_queue: WaitQueue::default(),
            write_wait_queue: WaitQueue::default(),
            epitems: SpinLock::new(LinkedList::new()),
        });
        let mut guard = result.inner.lock();
        guard.self_ref = Arc::downgrade(&result);
        // 释放锁
        drop(guard); //这一步其实不需要，只要离开作用域，guard生命周期结束，自会解锁
        return result;
    }

    pub fn inner(&self) -> &SpinLock<InnerPipeInode> {
        &self.inner
    }

    fn readable(&self) -> bool {
        let inode = self.inner.lock();
        return inode.valid_cnt > 0 || inode.writer == 0;
    }

    fn writeable(&self) -> bool {
        let inode = self.inner.lock();
        return !inode.buf_full() || inode.reader == 0;
    }

    pub fn add_epoll(&self, epitem: Arc<EPollItem>) -> Result<(), SystemError> {
        self.epitems.lock().push_back(epitem);
        Ok(())
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

impl IndexNode for LockedPipeInode {
    fn read_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &mut [u8],
        data_guard: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let data = data_guard.clone();
        drop(data_guard);
        // 获取mode
        let mode: FileMode;
        if let FilePrivateData::Pipefs(pdata) = &data {
            mode = pdata.mode;
        } else {
            return Err(SystemError::EBADF);
        }

        if buf.len() < len {
            return Err(SystemError::EINVAL);
        }
        // log::debug!("pipe mode: {:?}", mode);
        // 加锁
        let mut inner_guard = self.inner.lock();

        // 如果管道里面没有数据，则唤醒写端，
        while inner_guard.valid_cnt == 0 {
            // 如果当前管道写者数为0，则返回EOF
            if inner_guard.writer == 0 {
                return Ok(0);
            }

            self.write_wait_queue
                .wakeup(Some(ProcessState::Blocked(true)));

            // 如果为非阻塞管道，直接返回错误
            if mode.contains(FileMode::O_NONBLOCK) {
                drop(inner_guard);
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }

            // 否则在读等待队列中睡眠，并释放锁
            drop(inner_guard);
            let r = wq_wait_event_interruptible!(self.read_wait_queue, self.readable(), {});
            if r.is_err() {
                ProcessManager::current_pcb()
                    .flags()
                    .insert(ProcessFlags::HAS_PENDING_SIGNAL);
                return Err(SystemError::ERESTARTSYS);
            }

            inner_guard = self.inner.lock();
        }

        let mut num = inner_guard.valid_cnt as usize;
        //决定要输出的字节
        let start = inner_guard.read_pos as usize;
        //如果读端希望读取的字节数大于有效字节数，则输出有效字节
        let mut end =
            (inner_guard.valid_cnt as usize + inner_guard.read_pos as usize) % PIPE_BUFF_SIZE;
        //如果读端希望读取的字节数少于有效字节数，则输出希望读取的字节
        if len < inner_guard.valid_cnt as usize {
            end = (len + inner_guard.read_pos as usize) % PIPE_BUFF_SIZE;
            num = len;
        }

        // 从管道拷贝数据到用户的缓冲区

        if end < start {
            buf[0..(PIPE_BUFF_SIZE - start)]
                .copy_from_slice(&inner_guard.data[start..PIPE_BUFF_SIZE]);
            buf[(PIPE_BUFF_SIZE - start)..num].copy_from_slice(&inner_guard.data[0..end]);
        } else {
            buf[0..num].copy_from_slice(&inner_guard.data[start..end]);
        }

        //更新读位置以及valid_cnt
        inner_guard.read_pos = (inner_guard.read_pos + num as i32) % PIPE_BUFF_SIZE as i32;
        inner_guard.valid_cnt -= num as i32;

        // 读完以后如果未读完，则唤醒下一个读者
        if inner_guard.valid_cnt > 0 {
            self.read_wait_queue
                .wakeup(Some(ProcessState::Blocked(true)));
        }

        //读完后解锁并唤醒等待在写等待队列中的进程
        self.write_wait_queue
            .wakeup(Some(ProcessState::Blocked(true)));
        let pollflag = EPollEventType::from_bits_truncate(inner_guard.poll(&data)? as u32);
        drop(inner_guard);
        // 唤醒epoll中等待的进程
        EventPoll::wakeup_epoll(&self.epitems, Some(pollflag))?;

        //返回读取的字节数
        return Ok(num);
    }

    fn open(
        &self,
        mut data: SpinLockGuard<FilePrivateData>,
        mode: &crate::filesystem::vfs::file::FileMode,
    ) -> Result<(), SystemError> {
        let accmode = mode.accmode();
        let mut guard = self.inner.lock();
        // 不能以读写方式打开管道
        if accmode == FileMode::O_RDWR.bits() {
            return Err(SystemError::EACCES);
        } else if accmode == FileMode::O_RDONLY.bits() {
            guard.reader += 1;
            guard.had_reader = true;
            // println!(
            //     "FIFO:     pipe try open in read mode with reader pid:{:?}",
            //     ProcessManager::current_pid()
            // );
        } else if accmode == FileMode::O_WRONLY.bits() {
            // println!(
            //     "FIFO:     pipe try open in write mode with {} reader, writer pid:{:?}",
            //     guard.reader,
            //     ProcessManager::current_pid()
            // );
            if guard.reader == 0 && mode.contains(FileMode::O_NONBLOCK) {
                return Err(SystemError::ENXIO);
            }
            guard.writer += 1;
        }

        // 设置mode
        *data = FilePrivateData::Pipefs(PipeFsPrivateData { mode: *mode });

        return Ok(());
    }

    fn metadata(&self) -> Result<crate::filesystem::vfs::Metadata, SystemError> {
        let inode = self.inner.lock();
        let mut metadata = inode.metadata.clone();
        metadata.size = inode.data.len() as i64;

        return Ok(metadata);
    }

    fn close(&self, data: SpinLockGuard<FilePrivateData>) -> Result<(), SystemError> {
        let mode: FileMode;
        if let FilePrivateData::Pipefs(pipe_data) = &*data {
            mode = pipe_data.mode;
        } else {
            return Err(SystemError::EBADF);
        }
        let accmode = mode.accmode();
        let mut guard = self.inner.lock();

        // 写端关闭
        if accmode == FileMode::O_WRONLY.bits() {
            assert!(guard.writer > 0);
            guard.writer -= 1;
            // 如果已经没有写端了，则唤醒读端
            if guard.writer == 0 {
                self.read_wait_queue
                    .wakeup_all(Some(ProcessState::Blocked(true)));
            }
        }

        // 读端关闭
        if accmode == FileMode::O_RDONLY.bits() {
            assert!(guard.reader > 0);
            guard.reader -= 1;
            // 如果已经没有写端了，则唤醒读端
            if guard.reader == 0 {
                self.write_wait_queue
                    .wakeup_all(Some(ProcessState::Blocked(true)));
            }
        }

        return Ok(());
    }

    fn write_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &[u8],
        data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        // 获取mode
        let mode: FileMode;
        if let FilePrivateData::Pipefs(pdata) = &*data {
            mode = pdata.mode;
        } else {
            return Err(SystemError::EBADF);
        }

        if buf.len() < len || len > PIPE_BUFF_SIZE {
            return Err(SystemError::EINVAL);
        }
        // 加锁
        let mut inner_guard = self.inner.lock();

        if inner_guard.reader == 0 {
            if !inner_guard.had_reader {
                // 如果从未有读端，直接返回 ENXIO，无论是否阻塞模式
                return Err(SystemError::ENXIO);
            } else {
                // 如果曾经有读端，现在已关闭
                match mode.contains(FileMode::O_NONBLOCK) {
                    true => {
                        // 非阻塞模式，直接返回 EPIPE
                        return Err(SystemError::EPIPE);
                    }
                    false => {
                        let sig = Signal::SIGPIPE;
                        let mut info = SigInfo::new(
                            sig,
                            0,
                            SigCode::Kernel,
                            SigType::Kill(ProcessManager::current_pid()),
                        );
                        compiler_fence(core::sync::atomic::Ordering::SeqCst);

                        let _retval = sig
                            .send_signal_info(Some(&mut info), ProcessManager::current_pid())
                            .map(|x| x as usize);

                        compiler_fence(core::sync::atomic::Ordering::SeqCst);
                        return Err(SystemError::EPIPE);
                    }
                }
            }
        }

        // 如果管道空间不够

        while len + inner_guard.valid_cnt as usize > PIPE_BUFF_SIZE {
            // 唤醒读端
            self.read_wait_queue
                .wakeup(Some(ProcessState::Blocked(true)));

            // 如果为非阻塞管道，直接返回错误
            if mode.contains(FileMode::O_NONBLOCK) {
                drop(inner_guard);
                return Err(SystemError::ENOMEM);
            }

            // 解锁并睡眠
            drop(inner_guard);
            let r = wq_wait_event_interruptible!(self.write_wait_queue, self.writeable(), {});
            if r.is_err() {
                return Err(SystemError::ERESTARTSYS);
            }
            inner_guard = self.inner.lock();
        }

        // 决定要输入的字节
        let start = inner_guard.write_pos as usize;
        let end = (inner_guard.write_pos as usize + len) % PIPE_BUFF_SIZE;
        // 从用户的缓冲区拷贝数据到管道

        if end < start {
            inner_guard.data[start..PIPE_BUFF_SIZE]
                .copy_from_slice(&buf[0..(PIPE_BUFF_SIZE - start)]);
            inner_guard.data[0..end].copy_from_slice(&buf[(PIPE_BUFF_SIZE - start)..len]);
        } else {
            inner_guard.data[start..end].copy_from_slice(&buf[0..len]);
        }
        // 更新写位置以及valid_cnt
        inner_guard.write_pos = (inner_guard.write_pos + len as i32) % PIPE_BUFF_SIZE as i32;
        inner_guard.valid_cnt += len as i32;

        // 写完后还有位置，则唤醒下一个写者
        if (inner_guard.valid_cnt as usize) < PIPE_BUFF_SIZE {
            self.write_wait_queue
                .wakeup(Some(ProcessState::Blocked(true)));
        }

        // 读完后解锁并唤醒等待在读等待队列中的进程
        self.read_wait_queue
            .wakeup(Some(ProcessState::Blocked(true)));

        let pollflag = EPollEventType::from_bits_truncate(inner_guard.poll(&data)? as u32);

        drop(inner_guard);
        // 唤醒epoll中等待的进程
        EventPoll::wakeup_epoll(&self.epitems, Some(pollflag))?;

        // 返回写入的字节数
        return Ok(len);
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn get_entry_name_and_metadata(
        &self,
        ino: crate::filesystem::vfs::InodeId,
    ) -> Result<(alloc::string::String, crate::filesystem::vfs::Metadata), SystemError> {
        // 如果有条件，请在文件系统中使用高效的方式实现本接口，而不是依赖这个低效率的默认实现。
        let name = self.get_entry_name(ino)?;
        let entry = self.find(&name)?;
        return Ok((name, entry.metadata()?));
    }

    fn fs(&self) -> Arc<(dyn FileSystem)> {
        todo!()
    }

    fn list(&self) -> Result<alloc::vec::Vec<alloc::string::String>, SystemError> {
        return Err(SystemError::ENOSYS);
    }

    fn poll(&self, private_data: &FilePrivateData) -> Result<usize, SystemError> {
        return self.inner.lock().poll(private_data);
    }
}
