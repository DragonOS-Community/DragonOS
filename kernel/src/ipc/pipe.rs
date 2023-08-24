use crate::{
    arch::{sched::sched, CurrentIrqArch},
    exception::InterruptArch,
    filesystem::vfs::{
        core::generate_inode_id, FilePrivateData, FileSystem, FileType, IndexNode, Metadata,
        PollStatus,
    },
    libs::{spinlock::SpinLock, wait_queue::WaitQueue},
    process::ProcessState,
    syscall::SystemError,
    time::TimeSpec,
};

use alloc::sync::{Arc, Weak};

/// 我们设定pipe_buff的总大小为1024字节
const PIPE_BUFF_SIZE: usize = 1024;

/// @brief 管道文件i节点(锁)
#[derive(Debug)]
pub struct LockedPipeInode(SpinLock<InnerPipeInode>);

/// @brief 管道文件i节点(无锁)
#[derive(Debug)]
pub struct InnerPipeInode {
    self_ref: Weak<LockedPipeInode>,
    valid_cnt: i32,
    read_pos: i32,
    write_pos: i32,
    read_wait_queue: WaitQueue,
    write_wait_queue: WaitQueue,
    data: [u8; PIPE_BUFF_SIZE],
    /// INode 元数据
    metadata: Metadata,
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
                size: 0,
                blk_size: 0,
                blocks: 0,
                atime: TimeSpec::default(),
                mtime: TimeSpec::default(),
                ctime: TimeSpec::default(),
                file_type: FileType::Pipe,
                mode: 0o666,
                nlinks: 1,
                uid: 0,
                gid: 0,
                raw_dev: 0,
            },
        };
        let result = Arc::new(Self(SpinLock::new(inner)));
        let mut guard = result.0.lock();
        guard.self_ref = Arc::downgrade(&result);
        // 释放锁
        drop(guard); //这一步其实不需要，只要离开作用域，guard生命周期结束，自会解锁
        return result;
    }
}

impl IndexNode for LockedPipeInode {
    fn read_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: &mut FilePrivateData,
    ) -> Result<usize, crate::syscall::SystemError> {
        if buf.len() < len {
            return Err(SystemError::EINVAL);
        }
        // 加锁
        let mut inode = self.0.lock();

        //如果管道里面没有数据，则唤醒写端，
        while inode.valid_cnt == 0 {
            inode
                .write_wait_queue
                .wakeup(Some(ProcessState::Blocked(true)));

            // 在读等待队列中睡眠，并释放锁
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

        //读完后解锁并唤醒等待在写等待队列中的进程
        inode
            .write_wait_queue
            .wakeup(Some(ProcessState::Blocked(true)));
        //返回读取的字节数
        return Ok(num);
    }

    fn open(
        &self,
        _data: &mut FilePrivateData,
        _mode: &crate::filesystem::vfs::file::FileMode,
    ) -> Result<(), SystemError> {
        return Ok(());
    }

    fn metadata(&self) -> Result<crate::filesystem::vfs::Metadata, SystemError> {
        let inode = self.0.lock();
        let mut metadata = inode.metadata.clone();
        metadata.size = inode.data.len() as i64;

        return Ok(metadata);
    }

    fn close(&self, _data: &mut FilePrivateData) -> Result<(), SystemError> {
        return Ok(());
    }

    fn write_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &[u8],
        _data: &mut FilePrivateData,
    ) -> Result<usize, crate::syscall::SystemError> {
        if buf.len() < len || len > PIPE_BUFF_SIZE {
            return Err(SystemError::EINVAL);
        }
        // 加锁

        let mut inode = self.0.lock();

        // 如果管道空间不够

        while len + inode.valid_cnt as usize > PIPE_BUFF_SIZE {
            // 唤醒读端
            inode
                .read_wait_queue
                .wakeup(Some(ProcessState::Blocked(true)));
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

        // 读完后解锁并唤醒等待在读等待队列中的进程
        inode
            .read_wait_queue
            .wakeup(Some(ProcessState::Blocked(true)));
        // 返回写入的字节数
        return Ok(len);
    }

    fn poll(&self) -> Result<PollStatus, crate::syscall::SystemError> {
        return Ok(PollStatus::READ | PollStatus::WRITE);
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
}
