use alloc::sync::{Arc, Weak};
use crate::arch::sched::sched;
use crate::filesystem::vfs::core::generate_inode_id;
use crate::filesystem::vfs::{FilePrivateData, FileSystem, IndexNode, PollStatus, Metadata, FileType};
use crate::include::bindings::bindings::{PROC_INTERRUPTIBLE};
use crate::libs::{spinlock::SpinLock, wait_queue::WaitQueue};
use crate::syscall::SystemError;
use crate::time::TimeSpec;



/// 我们设定pipe_buff的总大小为1024字节
const PIPE_BUFF_SIZE: usize = 1024 ;


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
    data:[u8;PIPE_BUFF_SIZE],
    /// INode 元数据
    metadata: Metadata,
    
}

impl LockedPipeInode {
    pub fn new() -> Arc<Self> {
        let inner = InnerPipeInode {
            self_ref: Weak::default(),
            valid_cnt:0,
            read_pos: 0,
            write_pos: 0,
            read_wait_queue: WaitQueue::INIT,
            write_wait_queue: WaitQueue::INIT,
            data:[0;PIPE_BUFF_SIZE],
            
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
         drop(guard);//这一步其实不需要，只要离开作用域，guard生命周期结束，自会解锁
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
                .wakeup(PROC_INTERRUPTIBLE.into());
            
             // 在读等待队列中睡眠，并释放锁
           inode.read_wait_queue.sleep_without_sched();
            drop(inode);
            sched();
            inode=self.0.lock();
           
            }
            
            let mut num=inode.valid_cnt as usize;
            //决定要输出的字节
            let start=inode.read_pos as usize;
            //如果读端希望读取的字节数大于有效字节数，则输出有效字节
            let mut end=(inode.valid_cnt as usize+inode.read_pos as usize)% PIPE_BUFF_SIZE ;
            //如果读端希望读取的字节数少于有效字节数，则输出希望读取的字节
            if len<inode.valid_cnt as usize{
                 end=(len+inode.read_pos as usize)% PIPE_BUFF_SIZE ;
                 num=len;
            }
            
            // 从管道拷贝数据到用户的缓冲区
            let mut src = [0 as u8;PIPE_BUFF_SIZE];
            if end<start{
                src[0..(PIPE_BUFF_SIZE-1-start)].copy_from_slice(&inode.data[start..PIPE_BUFF_SIZE-1]);
                src[(PIPE_BUFF_SIZE-start)..].copy_from_slice(&inode.data[0..end])
            }
            else{
                src[0..num].copy_from_slice(&inode.data[start..end]);
            }
            
            buf[0..num].copy_from_slice(&src[0..num]);
           
            //更新读位置以及valid_cnt
            inode.read_pos = (inode.read_pos + num as i32) % PIPE_BUFF_SIZE as i32;
            inode.valid_cnt-=num as i32;
            
            //读完后解锁并唤醒等待在写等待队列中的进程
            inode.write_wait_queue.wakeup(PROC_INTERRUPTIBLE.into());
            //返回读取的字节数
            return Ok(num);

    }
    fn open(&self, _data: &mut FilePrivateData, _mode: &crate::filesystem::vfs::file::FileMode) -> Result<(), SystemError> {  
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
        if buf.len() < len||len>PIPE_BUFF_SIZE {
            return Err(SystemError::EINVAL);
        }     
         // 加锁
         
         let mut inode = self.0.try_lock().unwrap();
         
         //如果管道空间不够
        
        while len+inode.valid_cnt as usize>PIPE_BUFF_SIZE{ 
            //唤醒读端
            inode.read_wait_queue.wakeup(PROC_INTERRUPTIBLE.into());
            //解锁并睡眠
            inode.write_wait_queue.sleep_without_sched();
            drop(inode);
            sched();
            inode=self.0.lock();
        }
        
        //决定要输入的字节
        let start=inode.write_pos as usize;
        let end=(inode.write_pos as usize+len)%PIPE_BUFF_SIZE;
        // 从用户的缓冲区拷贝数据到管道
        
        if end<start{
            inode.data[start..PIPE_BUFF_SIZE-1].copy_from_slice(&buf[0..(PIPE_BUFF_SIZE-1-start)]);
            inode.data[0..end].copy_from_slice(&buf[(PIPE_BUFF_SIZE-start)..len]);
            
        }
        else{
            inode.data[start..end].copy_from_slice(&buf[0..len]);
        }
        //更新写位置以及valid_cnt
        inode.write_pos = (inode.write_pos + len as i32) % PIPE_BUFF_SIZE as i32;
        inode.valid_cnt+=len as i32;
        
        //读完后解锁并唤醒等待在读等待队列中的进程
        inode.read_wait_queue.wakeup(PROC_INTERRUPTIBLE.into());
        //返回写入的字节数
        return Ok(len);
       
    }

    fn poll(&self) -> Result<PollStatus, crate::syscall::SystemError> {
        return Ok(PollStatus {
            flags: PollStatus::READ_MASK | PollStatus::WRITE_MASK,
        });
    }

 
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn list(&self) -> Result<alloc::vec::Vec<alloc::string::String>, crate::syscall::SystemError> {
        return Err(SystemError::ENOTSUP);
    }

    fn set_metadata(&self, _metadata: &crate::filesystem::vfs::Metadata) -> Result<(), SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::ENOTSUP);
    }

    fn resize(&self, _len: usize) -> Result<(), SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::ENOTSUP);
    }

    fn create(
        &self,
        name: &str,
        file_type: crate::filesystem::vfs::FileType,
        mode: u32,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        // 若文件系统没有实现此方法，则默认调用其create_with_data方法。如果仍未实现，则会得到一个Err(-ENOTSUP)的返回值
        return self.create_with_data(name, file_type, mode, 0);
    }

    fn create_with_data(
        &self,
        _name: &str,
        _file_type: crate::filesystem::vfs::FileType,
        _mode: u32,
        _data: usize,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::ENOTSUP);
    }

    fn link(&self, _name: &str, _other: &Arc<dyn IndexNode>) -> Result<(), SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::ENOTSUP);
    }

    fn unlink(&self, _name: &str) -> Result<(), SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::ENOTSUP);
    }

    fn rmdir(&self, _name: &str) ->Result<(), SystemError>{
        return Err(SystemError::ENOTSUP);
    }

    fn move_(
        &self,
        _old_name: &str,
        _target: &Arc<dyn IndexNode>,
        _new_name: &str,
    ) -> Result<(), SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::ENOTSUP);
    }

    fn find(&self, _name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::ENOTSUP);
    }

    fn get_entry_name(&self, _ino: crate::filesystem::vfs::InodeId) -> Result<alloc::string::String, SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::ENOTSUP);
    }

    fn get_entry_name_and_metadata(&self, ino: crate::filesystem::vfs::InodeId) -> Result<(alloc::string::String, crate::filesystem::vfs::Metadata), SystemError> {
        // 如果有条件，请在文件系统中使用高效的方式实现本接口，而不是依赖这个低效率的默认实现。
        let name = self.get_entry_name(ino)?;
        let entry = self.find(&name)?;
        return Ok((name, entry.metadata()?));
    }

    fn ioctl(&self, _cmd: u32, _data: usize) -> Result<usize, SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::ENOTSUP);
    }

    fn mount(&self, _fs: Arc<dyn FileSystem>) -> Result<Arc<crate::filesystem::vfs::MountFS>, SystemError> {
        return Err(SystemError::ENOTSUP);
    }

    fn truncate(&self, _len: usize) -> Result<(), SystemError> {
        return Err(SystemError::ENOTSUP);
    }

    fn sync(&self) -> Result<(), SystemError> {
        return Ok(());
    }

    fn fs(&self) -> Arc<(dyn FileSystem )> {
        todo!()
    }
}
