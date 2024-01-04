use crate::{
    arch::{sched::sched, CurrentIrqArch},
    exception::InterruptArch,
    filesystem::vfs::{
        core::generate_inode_id, file::FileMode, syscall::ModeType, FilePrivateData, FileSystem,
        FileType, IndexNode, Metadata,
    },
    libs::{spinlock::SpinLock, wait_queue::WaitQueue},
    net::event_poll::{EPollEventType, EPollItem, EventPoll},
    process::ProcessState,
    time::TimeSpec,
};

use alloc::{
    collections::LinkedList,
    sync::{Arc, Weak},
};
use system_error::SystemError;

/// 我们设定pipe_buff的总大小为1024字节
const PIPE_BUFF_SIZE: usize = 1024;

#[derive(Debug, Clone)]
pub struct PipeFsPrivateData {
    mode: FileMode,
}

impl PipeFsPrivateData {
    pub fn new(mode: FileMode) -> Self {
        return PipeFsPrivateData { mode: mode };
    }

    pub fn set_mode(&mut self, mode: FileMode) {
        self.mode = mode;
    }
}

/// @brief 管道文件i节点(锁)
#[derive(Debug)]
pub struct LockedPipeInode(SpinLock<InnerPipeInode>);

/// @brief 管道文件i节点(无锁)
#[derive(Debug)]
pub struct InnerPipeInode {
    self_ref: Weak<LockedPipeInode>,
    /// 管道内可读的数据数
    valid_cnt: i32,
    read_pos: i32,
    write_pos: i32,
    read_wait_queue: WaitQueue,
    write_wait_queue: WaitQueue,
    data: [u8; PIPE_BUFF_SIZE],
    /// INode 元数据
    metadata: Metadata,
    reader: u32,
    writer: u32,
    epitems: SpinLock<LinkedList<Arc<EPollItem>>>,
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
                events.insert(EPollEventType::EPOLLIN & EPollEventType::EPOLLRDNORM);
            }

            // 没有写者
            if self.writer == 0 {
                events.insert(EPollEventType::EPOLLHUP)
            }
        }

        if mode.contains(FileMode::O_WRONLY) {
            // 管道内数据未满
            if self.valid_cnt as usize != PIPE_BUFF_SIZE {
                events.insert(EPollEventType::EPOLLIN & EPollEventType::EPOLLWRNORM);
            }

            // 没有读者
            if self.reader == 0 {
                events.insert(EPollEventType::EPOLLERR);
            }
        }

        Ok(events.bits() as usize)
    }

    pub fn add_epoll(&mut self, epitem: Arc<EPollItem>) -> Result<(), SystemError> {
        self.epitems.lock().push_back(epitem);
        Ok(())
    }
}

impl LockedPipeInode {
    pub fn new() -> Arc<Self> {
        let inner = InnerPipeInode {
            self_ref: Weak::default(),
            valid_cnt: 0,
            read_pos: 0,
            write_pos: 0,
            read_wait_queue: WaitQueue::INIT,
            write_wait_queue: WaitQueue::INIT,
            data: [0; PIPE_BUFF_SIZE],

            metadata: Metadata {
                dev_id: 0,
                inode_id: generate_inode_id(),
                size: PIPE_BUFF_SIZE as i64,
                blk_size: 0,
                blocks: 0,
                atime: TimeSpec::default(),
                mtime: TimeSpec::default(),
                ctime: TimeSpec::default(),
                file_type: FileType::Pipe,
                mode: ModeType::from_bits_truncate(0o666),
                nlinks: 1,
                uid: 0,
                gid: 0,
                raw_dev: Default::default(),
            },
            reader: 0,
            writer: 0,
            epitems: SpinLock::new(LinkedList::new()),
        };
        let result = Arc::new(Self(SpinLock::new(inner)));
        let mut guard = result.0.lock();
        guard.self_ref = Arc::downgrade(&result);
        // 释放锁
        drop(guard); //这一步其实不需要，只要离开作用域，guard生命周期结束，自会解锁
        return result;
    }

    pub fn inner(&self) -> &SpinLock<InnerPipeInode> {
        &self.0
    }
}

impl IndexNode for LockedPipeInode {
    fn read_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &mut [u8],
        data: &mut FilePrivateData,
    ) -> Result<usize, SystemError> {
        // 获取mode
        let mode: FileMode;
        if let FilePrivateData::Pipefs(pdata) = data {
            mode = pdata.mode;
        } else {
            return Err(SystemError::EBADF);
        }

        if buf.len() < len {
            return Err(SystemError::EINVAL);
        }
        // 加锁
        let mut inode = self.0.lock();

        // 如果管道里面没有数据，则唤醒写端，
        while inode.valid_cnt == 0 {
            // 如果当前管道写者数为0，则返回EOF
            if inode.writer == 0 {
                return Ok(0);
            }

            inode
                .write_wait_queue
                .wakeup(Some(ProcessState::Blocked(true)));

            // 如果为非阻塞管道，直接返回错误
            if mode.contains(FileMode::O_NONBLOCK) {
                drop(inode);
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }

            // 否则在读等待队列中睡眠，并释放锁
            unsafe {
                let irq_guard = CurrentIrqArch::save_and_disable_irq();

                inode.read_wait_queue.sleep_without_schedule();
                drop(inode);

                drop(irq_guard);
            }
            sched();
            inode = self.0.lock();
        }

        let mut num = inode.valid_cnt as usize;
        //决定要输出的字节
        let start = inode.read_pos as usize;
        //如果读端希望读取的字节数大于有效字节数，则输出有效字节
        let mut end = (inode.valid_cnt as usize + inode.read_pos as usize) % PIPE_BUFF_SIZE;
        //如果读端希望读取的字节数少于有效字节数，则输出希望读取的字节
        if len < inode.valid_cnt as usize {
            end = (len + inode.read_pos as usize) % PIPE_BUFF_SIZE;
            num = len;
        }

        // 从管道拷贝数据到用户的缓冲区

        if end < start {
            buf[0..(PIPE_BUFF_SIZE - start)].copy_from_slice(&inode.data[start..PIPE_BUFF_SIZE]);
            buf[(PIPE_BUFF_SIZE - start)..num].copy_from_slice(&inode.data[0..end]);
        } else {
            buf[0..num].copy_from_slice(&inode.data[start..end]);
        }

        //更新读位置以及valid_cnt
        inode.read_pos = (inode.read_pos + num as i32) % PIPE_BUFF_SIZE as i32;
        inode.valid_cnt -= num as i32;

        // 读完以后如果未读完，则唤醒下一个读者
        if inode.valid_cnt > 0 {
            inode
                .read_wait_queue
                .wakeup(Some(ProcessState::Blocked(true)));
        }

        //读完后解锁并唤醒等待在写等待队列中的进程
        inode
            .write_wait_queue
            .wakeup(Some(ProcessState::Blocked(true)));

        let pollflag = EPollEventType::from_bits_truncate(inode.poll(&data)? as u32);
        // 唤醒epoll中等待的进程
        EventPoll::wakeup_epoll(&mut inode.epitems, pollflag)?;

        //返回读取的字节数
        return Ok(num);
    }

    fn open(
        &self,
        data: &mut FilePrivateData,
        mode: &crate::filesystem::vfs::file::FileMode,
    ) -> Result<(), SystemError> {
        let mut guard = self.0.lock();
        // 不能以读写方式打开管道
        if mode.contains(FileMode::O_RDWR) {
            return Err(SystemError::EACCES);
        }
        if mode.contains(FileMode::O_RDONLY) {
            guard.reader += 1;
        }
        if mode.contains(FileMode::O_WRONLY) {
            guard.writer += 1;
        }

        // 设置mode
        *data = FilePrivateData::Pipefs(PipeFsPrivateData { mode: *mode });

        return Ok(());
    }

    fn metadata(&self) -> Result<crate::filesystem::vfs::Metadata, SystemError> {
        let inode = self.0.lock();
        let mut metadata = inode.metadata.clone();
        metadata.size = inode.data.len() as i64;

        return Ok(metadata);
    }

    fn close(&self, data: &mut FilePrivateData) -> Result<(), SystemError> {
        let mode: FileMode;
        if let FilePrivateData::Pipefs(pipe_data) = data {
            mode = pipe_data.mode;
        } else {
            return Err(SystemError::EBADF);
        }
        let mut guard = self.0.lock();

        // 写端关闭
        if mode.contains(FileMode::O_WRONLY) {
            assert!(guard.writer > 0);
            guard.writer -= 1;
            // 如果已经没有写端了，则唤醒读端
            if guard.writer == 0 {
                guard
                    .read_wait_queue
                    .wakeup_all(Some(ProcessState::Blocked(true)));
            }
        }

        // 读端关闭
        if mode.contains(FileMode::O_RDONLY) {
            assert!(guard.reader > 0);
            guard.reader -= 1;
            // 如果已经没有写端了，则唤醒读端
            if guard.reader == 0 {
                guard
                    .write_wait_queue
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
        data: &mut FilePrivateData,
    ) -> Result<usize, SystemError> {
        // 获取mode
        let mode: FileMode;
        if let FilePrivateData::Pipefs(pdata) = data {
            mode = pdata.mode;
        } else {
            return Err(SystemError::EBADF);
        }

        if buf.len() < len || len > PIPE_BUFF_SIZE {
            return Err(SystemError::EINVAL);
        }
        // 加锁

        let mut inode = self.0.lock();

        // TODO: 如果已经没有读端存在了，则向写端进程发送SIGPIPE信号
        if inode.reader == 0 {}

        // 如果管道空间不够

        while len + inode.valid_cnt as usize > PIPE_BUFF_SIZE {
            // 唤醒读端
            inode
                .read_wait_queue
                .wakeup(Some(ProcessState::Blocked(true)));

            // 如果为非阻塞管道，直接返回错误
            if mode.contains(FileMode::O_NONBLOCK) {
                drop(inode);
                return Err(SystemError::ENOMEM);
            }

            // 解锁并睡眠
            unsafe {
                let irq_guard = CurrentIrqArch::save_and_disable_irq();
                inode.write_wait_queue.sleep_without_schedule();
                drop(inode);
                drop(irq_guard);
            }
            sched();
            inode = self.0.lock();
        }

        // 决定要输入的字节
        let start = inode.write_pos as usize;
        let end = (inode.write_pos as usize + len) % PIPE_BUFF_SIZE;
        // 从用户的缓冲区拷贝数据到管道

        if end < start {
            inode.data[start..PIPE_BUFF_SIZE].copy_from_slice(&buf[0..(PIPE_BUFF_SIZE - start)]);
            inode.data[0..end].copy_from_slice(&buf[(PIPE_BUFF_SIZE - start)..len]);
        } else {
            inode.data[start..end].copy_from_slice(&buf[0..len]);
        }
        // 更新写位置以及valid_cnt
        inode.write_pos = (inode.write_pos + len as i32) % PIPE_BUFF_SIZE as i32;
        inode.valid_cnt += len as i32;

        // 写完后还有位置，则唤醒下一个写者
        if (inode.valid_cnt as usize) < PIPE_BUFF_SIZE {
            inode
                .write_wait_queue
                .wakeup(Some(ProcessState::Blocked(true)));
        }

        // 读完后解锁并唤醒等待在读等待队列中的进程
        inode
            .read_wait_queue
            .wakeup(Some(ProcessState::Blocked(true)));

        let pollflag = EPollEventType::from_bits_truncate(inode.poll(&data)? as u32);
        // 唤醒epoll中等待的进程
        EventPoll::wakeup_epoll(&mut inode.epitems, pollflag)?;

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
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    fn poll(&self, private_data: &FilePrivateData) -> Result<usize, SystemError> {
        return self.0.lock().poll(private_data);
    }
}
