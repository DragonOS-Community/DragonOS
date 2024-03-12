use alloc::{
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

use crate::{
    driver::{
        base::{block::SeekFrom, device::DevicePrivateData},
        tty::tty_device::TtyFilePrivateData,
    },
    filesystem::procfs::ProcfsFilePrivateData,
    ipc::pipe::{LockedPipeInode, PipeFsPrivateData},
    kerror,
    libs::spinlock::SpinLock,
    net::{
        event_poll::{EPollItem, EPollPrivateData, EventPoll},
        socket::SocketInode,
    },
    process::ProcessManager,
};

use super::{Dirent, FileType, IndexNode, InodeId, Metadata, SpecialNodeData};

/// 文件私有信息的枚举类型
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum FilePrivateData {
    /// 管道文件私有信息
    Pipefs(PipeFsPrivateData),
    /// procfs文件私有信息
    Procfs(ProcfsFilePrivateData),
    /// 设备文件的私有信息
    DevFS(DevicePrivateData),
    /// tty设备文件的私有信息
    Tty(TtyFilePrivateData),
    /// epoll私有信息
    EPoll(EPollPrivateData),
    /// 不需要文件私有信息
    Unused,
}

impl Default for FilePrivateData {
    fn default() -> Self {
        return Self::Unused;
    }
}

impl FilePrivateData {
    pub fn update_mode(&mut self, mode: FileMode) {
        if let FilePrivateData::Pipefs(pdata) = self {
            pdata.set_mode(mode);
        }
    }
}

bitflags! {
    /// @brief 文件打开模式
    /// 其中，低2bit组合而成的数字的值，用于表示访问权限。其他的bit，才支持通过按位或的方式来表示参数
    ///
    /// 与Linux 5.19.10的uapi/asm-generic/fcntl.h相同
    /// https://code.dragonos.org.cn/xref/linux-5.19.10/tools/include/uapi/asm-generic/fcntl.h#19
    pub struct FileMode: u32{
        /* File access modes for `open' and `fcntl'.  */
        /// Open Read-only
        const O_RDONLY = 0o0;
        /// Open Write-only
        const O_WRONLY = 0o1;
        /// Open read/write
        const O_RDWR = 0o2;
        /// Mask for file access modes
        const O_ACCMODE = 0o00000003;

        /* Bits OR'd into the second argument to open.  */
        /// Create file if it does not exist
        const O_CREAT = 0o00000100;
        /// Fail if file already exists
        const O_EXCL = 0o00000200;
        /// Do not assign controlling terminal
        const O_NOCTTY = 0o00000400;
        /// 文件存在且是普通文件，并以O_RDWR或O_WRONLY打开，则它会被清空
        const O_TRUNC = 0o00001000;
        /// 文件指针会被移动到文件末尾
        const O_APPEND = 0o00002000;
        /// 非阻塞式IO模式
        const O_NONBLOCK = 0o00004000;
        /// 每次write都等待物理I/O完成，但是如果写操作不影响读取刚写入的数据，则不等待文件属性更新
        const O_DSYNC = 0o00010000;
        /// fcntl, for BSD compatibility
        const FASYNC = 0o00020000;
        /* direct disk access hint */
        const O_DIRECT = 0o00040000;
        const O_LARGEFILE = 0o00100000;
        /// 打开的必须是一个目录
        const O_DIRECTORY = 0o00200000;
        /// Do not follow symbolic links
        const O_NOFOLLOW = 0o00400000;
        const O_NOATIME = 0o01000000;
        /// set close_on_exec
        const O_CLOEXEC = 0o02000000;
        /// 每次write都等到物理I/O完成，包括write引起的文件属性的更新
        const O_SYNC = 0o04000000;

        const O_PATH = 0o10000000;

        const O_PATH_FLAGS = Self::O_DIRECTORY.bits|Self::O_NOFOLLOW.bits|Self::O_CLOEXEC.bits|Self::O_PATH.bits;
    }
}

impl FileMode {
    /// @brief 获取文件的访问模式的值
    #[inline]
    pub fn accmode(&self) -> u32 {
        return self.bits() & FileMode::O_ACCMODE.bits();
    }
}
/// @brief 抽象文件结构体
#[derive(Debug)]
pub struct File {
    inode: Arc<dyn IndexNode>,
    /// 对于文件，表示字节偏移量；对于文件夹，表示当前操作的子目录项偏移量
    offset: usize,
    /// 文件的打开模式
    mode: FileMode,
    /// 文件类型
    file_type: FileType,
    /// readdir时候用的，暂存的本次循环中，所有子目录项的名字的数组
    readdir_subdirs_name: Vec<String>,
    pub private_data: FilePrivateData,
}

impl File {
    /// @brief 创建一个新的文件对象
    ///
    /// @param inode 文件对象对应的inode
    /// @param mode 文件的打开模式
    pub fn new(inode: Arc<dyn IndexNode>, mode: FileMode) -> Result<Self, SystemError> {
        let mut inode = inode;
        let file_type = inode.metadata()?.file_type;
        match file_type {
            FileType::Pipe => {
                if let Some(SpecialNodeData::Pipe(pipe_inode)) = inode.special_node() {
                    inode = pipe_inode;
                }
            }
            _ => {}
        }

        let mut f = File {
            inode,
            offset: 0,
            mode,
            file_type,
            readdir_subdirs_name: Vec::new(),
            private_data: FilePrivateData::default(),
        };
        // kdebug!("inode:{:?}",f.inode);
        f.inode.open(&mut f.private_data, &mode)?;

        return Ok(f);
    }

    /// @brief 从文件中读取指定的字节数到buffer中
    ///
    /// @param len 要读取的字节数
    /// @param buf 目标buffer
    ///
    /// @return Ok(usize) 成功读取的字节数
    /// @return Err(SystemError) 错误码
    pub fn read(&mut self, len: usize, buf: &mut [u8]) -> Result<usize, SystemError> {
        self.do_read(self.offset, len, buf, true)
    }

    /// @brief 从buffer向文件写入指定的字节数的数据
    ///
    /// @param len 要写入的字节数
    /// @param buf 源数据buffer
    ///
    /// @return Ok(usize) 成功写入的字节数
    /// @return Err(SystemError) 错误码
    pub fn write(&mut self, len: usize, buf: &[u8]) -> Result<usize, SystemError> {
        self.do_write(self.offset, len, buf, true)
    }

    /// ## 从文件中指定的偏移处读取指定的字节数到buf中
    ///
    /// ### 参数
    /// - `offset`: 文件偏移量
    /// - `len`: 要读取的字节数
    /// - `buf`: 读出缓冲区
    ///
    /// ### 返回值
    /// - `Ok(usize)`: 成功读取的字节数
    pub fn pread(
        &mut self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
    ) -> Result<usize, SystemError> {
        self.do_read(offset, len, buf, false)
    }

    /// ## 从buf向文件中指定的偏移处写入指定的字节数的数据
    ///
    /// ### 参数
    /// - `offset`: 文件偏移量
    /// - `len`: 要写入的字节数
    /// - `buf`: 写入缓冲区
    ///
    /// ### 返回值
    /// - `Ok(usize)`: 成功写入的字节数
    pub fn pwrite(&mut self, offset: usize, len: usize, buf: &[u8]) -> Result<usize, SystemError> {
        self.do_write(offset, len, buf, false)
    }

    fn do_read(
        &mut self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        update_offset: bool,
    ) -> Result<usize, SystemError> {
        // 先检查本文件在权限等规则下，是否可读取。
        self.readable()?;
        if buf.len() < len {
            return Err(SystemError::ENOBUFS);
        }

        let len = self
            .inode
            .read_at(offset, len, buf, &mut self.private_data)?;

        if update_offset {
            self.offset += len;
        }

        Ok(len)
    }

    fn do_write(
        &mut self,
        offset: usize,
        len: usize,
        buf: &[u8],
        update_offset: bool,
    ) -> Result<usize, SystemError> {
        // 先检查本文件在权限等规则下，是否可写入。
        self.writeable()?;
        if buf.len() < len {
            return Err(SystemError::ENOBUFS);
        }

        // 如果文件指针已经超过了文件大小，则需要扩展文件大小
        if offset > self.inode.metadata()?.size as usize {
            self.inode.resize(offset)?;
        }
        let len = self
            .inode
            .write_at(offset, len, buf, &mut self.private_data)?;

        if update_offset {
            self.offset += len;
        }

        Ok(len)
    }

    /// @brief 获取文件的元数据
    pub fn metadata(&self) -> Result<Metadata, SystemError> {
        return self.inode.metadata();
    }

    /// @brief 根据inode号获取子目录项的名字
    #[allow(dead_code)]
    pub fn get_entry_name(&self, ino: InodeId) -> Result<String, SystemError> {
        return self.inode.get_entry_name(ino);
    }

    /// @brief 调整文件操作指针的位置
    ///
    /// @param origin 调整的起始位置
    pub fn lseek(&mut self, origin: SeekFrom) -> Result<usize, SystemError> {
        let file_type = self.inode.metadata()?.file_type;
        match file_type {
            FileType::Pipe | FileType::CharDevice => {
                return Err(SystemError::ESPIPE);
            }
            _ => {}
        }

        let pos: i64;
        match origin {
            SeekFrom::SeekSet(offset) => {
                pos = offset;
            }
            SeekFrom::SeekCurrent(offset) => {
                pos = self.offset as i64 + offset;
            }
            SeekFrom::SeekEnd(offset) => {
                let metadata = self.metadata()?;
                pos = metadata.size + offset;
            }
            SeekFrom::Invalid => {
                return Err(SystemError::EINVAL);
            }
        }
        // 根据linux man page, lseek允许超出文件末尾，并且不改变文件大小
        // 当pos超出文件末尾时，read返回0。直到开始写入数据时，才会改变文件大小
        if pos < 0 {
            return Err(SystemError::EOVERFLOW);
        }
        self.offset = pos as usize;
        return Ok(self.offset);
    }

    /// @brief 判断当前文件是否可读
    #[inline]
    pub fn readable(&self) -> Result<(), SystemError> {
        // 暂时认为只要不是write only, 就可读
        if self.mode == FileMode::O_WRONLY {
            return Err(SystemError::EPERM);
        }

        return Ok(());
    }

    /// @brief 判断当前文件是否可写
    #[inline]
    pub fn writeable(&self) -> Result<(), SystemError> {
        // 暂时认为只要不是read only, 就可写
        if self.mode == FileMode::O_RDONLY {
            return Err(SystemError::EPERM);
        }

        return Ok(());
    }

    /// @biref 充填dirent结构体
    /// @return 返回dirent结构体的大小
    pub fn readdir(&mut self, dirent: &mut Dirent) -> Result<u64, SystemError> {
        let inode: &Arc<dyn IndexNode> = &self.inode;

        // 如果偏移量为0
        if self.offset == 0 {
            // 通过list更新readdir_subdirs_name
            self.readdir_subdirs_name = inode.list()?;
            self.readdir_subdirs_name.sort();
        }
        // kdebug!("sub_entries={sub_entries:?}");

        // 已经读到末尾
        if self.offset == self.readdir_subdirs_name.len() {
            self.offset = 0;
            return Ok(0);
        }
        let name = &self.readdir_subdirs_name[self.offset];
        let sub_inode: Arc<dyn IndexNode> = match inode.find(&name) {
            Ok(i) => i,
            Err(e) => {
                kerror!(
                    "Readdir error: Failed to find sub inode:{name:?}, file={self:?}, error={e:?}"
                );
                return Err(e);
            }
        };

        let name_bytes: &[u8] = name.as_bytes();

        // 根据posix的规定，dirent中的d_name是一个不定长的数组，因此需要unsafe来拷贝数据
        unsafe {
            let ptr = &mut dirent.d_name as *mut u8;

            let buf: &mut [u8] =
                ::core::slice::from_raw_parts_mut::<'static, u8>(ptr, name_bytes.len() + 1);
            buf[0..name_bytes.len()].copy_from_slice(name_bytes);
            buf[name_bytes.len()] = 0;
        }

        self.offset += 1;
        dirent.d_ino = sub_inode.metadata().unwrap().inode_id.into() as u64;
        dirent.d_type = sub_inode.metadata().unwrap().file_type.get_file_type_num() as u8;

        // 计算dirent结构体的大小
        let size = (name_bytes.len() + ::core::mem::size_of::<Dirent>()
            - ::core::mem::size_of_val(&dirent.d_name)) as u64;

        dirent.d_reclen = size as u16;
        dirent.d_off += dirent.d_reclen as i64;

        return Ok(size);
    }

    pub fn inode(&self) -> Arc<dyn IndexNode> {
        return self.inode.clone();
    }

    /// @brief 尝试克隆一个文件
    ///
    /// @return Option<File> 克隆后的文件结构体。如果克隆失败，返回None
    pub fn try_clone(&self) -> Option<File> {
        let mut res = Self {
            inode: self.inode.clone(),
            offset: self.offset.clone(),
            mode: self.mode.clone(),
            file_type: self.file_type.clone(),
            readdir_subdirs_name: self.readdir_subdirs_name.clone(),
            private_data: self.private_data.clone(),
        };
        // 调用inode的open方法，让inode知道有新的文件打开了这个inode
        if self.inode.open(&mut res.private_data, &res.mode).is_err() {
            return None;
        }

        return Some(res);
    }

    /// @brief 获取文件的类型
    #[inline]
    pub fn file_type(&self) -> FileType {
        return self.file_type;
    }

    /// @brief 获取文件的打开模式
    #[inline]
    pub fn mode(&self) -> FileMode {
        return self.mode;
    }

    /// 获取文件是否在execve时关闭
    #[inline]
    pub fn close_on_exec(&self) -> bool {
        return self.mode.contains(FileMode::O_CLOEXEC);
    }

    /// 设置文件是否在execve时关闭
    #[inline]
    pub fn set_close_on_exec(&mut self, close_on_exec: bool) {
        if close_on_exec {
            self.mode.insert(FileMode::O_CLOEXEC);
        } else {
            self.mode.remove(FileMode::O_CLOEXEC);
        }
    }

    pub fn set_mode(&mut self, mode: FileMode) -> Result<(), SystemError> {
        // todo: 是否需要调用inode的open方法，以更新private data（假如它与mode有关的话）?
        // 也许需要加个更好的设计，让inode知晓文件的打开模式发生了变化，让它自己决定是否需要更新private data

        // 直接修改文件的打开模式
        self.mode = mode;
        self.private_data.update_mode(mode);
        return Ok(());
    }

    /// @brief 重新设置文件的大小
    ///
    /// 如果文件大小增加，则文件内容不变，但是文件的空洞部分会被填充为0
    /// 如果文件大小减小，则文件内容会被截断
    ///
    /// @return 成功：Ok()
    ///         失败：Err(错误码)
    pub fn ftruncate(&self, len: usize) -> Result<(), SystemError> {
        // 如果文件不可写，返回错误
        self.writeable()?;

        // 调用inode的truncate方法
        self.inode.resize(len)?;
        return Ok(());
    }

    /// ## 向该文件添加一个EPollItem对象
    ///
    /// 在文件状态发生变化时，需要向epoll通知
    pub fn add_epoll(&mut self, epitem: Arc<EPollItem>) -> Result<(), SystemError> {
        match self.file_type {
            FileType::Socket => {
                let inode = self.inode.downcast_ref::<SocketInode>().unwrap();
                let mut socket = inode.inner();

                return socket.add_epoll(epitem);
            }
            FileType::Pipe => {
                let inode = self.inode.downcast_ref::<LockedPipeInode>().unwrap();
                return inode.inner().lock().add_epoll(epitem);
            }
            _ => {
                let r = self.inode.ioctl(
                    EventPoll::ADD_EPOLLITEM,
                    &epitem as *const Arc<EPollItem> as usize,
                    &self.private_data,
                );
                if r.is_err() {
                    return Err(SystemError::ENOSYS);
                }

                Ok(())
            }
        }
    }

    /// ## 删除一个绑定的epoll
    pub fn remove_epoll(&mut self, epoll: &Weak<SpinLock<EventPoll>>) -> Result<(), SystemError> {
        match self.file_type {
            FileType::Socket => {
                let inode = self.inode.downcast_ref::<SocketInode>().unwrap();
                let mut socket = inode.inner();

                return socket.remove_epoll(epoll);
            }
            _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
        }
    }

    pub fn poll(&self) -> Result<usize, SystemError> {
        self.inode.poll(&self.private_data)
    }
}

impl Drop for File {
    fn drop(&mut self) {
        let r: Result<(), SystemError> = self.inode.close(&mut self.private_data);
        // 打印错误信息
        if r.is_err() {
            kerror!(
                "pid: {:?} failed to close file: {:?}, errno={:?}",
                ProcessManager::current_pcb().pid(),
                self,
                r.as_ref().unwrap_err()
            );
        }
    }
}

/// @brief pcb里面的文件描述符数组
#[derive(Debug)]
pub struct FileDescriptorVec {
    /// 当前进程打开的文件描述符
    fds: Vec<Option<Arc<SpinLock<File>>>>,
}

impl FileDescriptorVec {
    pub const PROCESS_MAX_FD: usize = 1024;

    #[inline(never)]
    pub fn new() -> FileDescriptorVec {
        let mut data = Vec::with_capacity(FileDescriptorVec::PROCESS_MAX_FD);
        data.resize(FileDescriptorVec::PROCESS_MAX_FD, None);

        // 初始化文件描述符数组结构体
        return FileDescriptorVec { fds: data };
    }

    /// @brief 克隆一个文件描述符数组
    ///
    /// @return FileDescriptorVec 克隆后的文件描述符数组
    pub fn clone(&self) -> FileDescriptorVec {
        let mut res = FileDescriptorVec::new();
        for i in 0..FileDescriptorVec::PROCESS_MAX_FD {
            if let Some(file) = &self.fds[i] {
                if let Some(file) = file.lock().try_clone() {
                    res.fds[i] = Some(Arc::new(SpinLock::new(file)));
                }
            }
        }
        return res;
    }

    /// @brief 判断文件描述符序号是否合法
    ///
    /// @return true 合法
    ///
    /// @return false 不合法
    #[inline]
    pub fn validate_fd(fd: i32) -> bool {
        if fd < 0 || fd as usize > FileDescriptorVec::PROCESS_MAX_FD {
            return false;
        } else {
            return true;
        }
    }

    /// 申请文件描述符，并把文件对象存入其中。
    ///
    /// ## 参数
    ///
    /// - `file` 要存放的文件对象
    /// - `fd` 如果为Some(i32)，表示指定要申请这个文件描述符，如果这个文件描述符已经被使用，那么返回EBADF
    ///
    /// ## 返回值
    ///
    /// - `Ok(i32)` 申请成功，返回申请到的文件描述符
    /// - `Err(SystemError)` 申请失败，返回错误码，并且，file对象将被drop掉
    pub fn alloc_fd(&mut self, file: File, fd: Option<i32>) -> Result<i32, SystemError> {
        if fd.is_some() {
            // 指定了要申请的文件描述符编号
            let new_fd = fd.unwrap();
            let x = &mut self.fds[new_fd as usize];
            if x.is_none() {
                *x = Some(Arc::new(SpinLock::new(file)));
                return Ok(new_fd);
            } else {
                return Err(SystemError::EBADF);
            }
        } else {
            // 没有指定要申请的文件描述符编号
            for i in 0..FileDescriptorVec::PROCESS_MAX_FD {
                if self.fds[i].is_none() {
                    self.fds[i] = Some(Arc::new(SpinLock::new(file)));
                    return Ok(i as i32);
                }
            }
            return Err(SystemError::EMFILE);
        }
    }

    /// 根据文件描述符序号，获取文件结构体的Arc指针
    ///
    /// ## 参数
    ///
    /// - `fd` 文件描述符序号
    pub fn get_file_by_fd(&self, fd: i32) -> Option<Arc<SpinLock<File>>> {
        if !FileDescriptorVec::validate_fd(fd) {
            return None;
        }
        return self.fds[fd as usize].clone();
    }

    /// 释放文件描述符，同时关闭文件。
    ///
    /// ## 参数
    ///
    /// - `fd` 文件描述符序号
    pub fn drop_fd(&mut self, fd: i32) -> Result<(), SystemError> {
        // 判断文件描述符的数字是否超过限制
        if !FileDescriptorVec::validate_fd(fd) {
            return Err(SystemError::EBADF);
        }

        self.get_file_by_fd(fd).ok_or(SystemError::EBADF)?;

        // 把文件描述符数组对应位置设置为空
        let file = self.fds[fd as usize].take().unwrap();

        assert!(Arc::strong_count(&file) == 1);
        return Ok(());
    }

    #[allow(dead_code)]
    pub fn iter(&self) -> FileDescriptorIterator {
        return FileDescriptorIterator::new(self);
    }

    pub fn close_on_exec(&mut self) {
        for i in 0..FileDescriptorVec::PROCESS_MAX_FD {
            if let Some(file) = &self.fds[i] {
                let to_drop = file.lock().close_on_exec();
                if to_drop {
                    if let Err(r) = self.drop_fd(i as i32) {
                        kerror!(
                            "Failed to close file: pid = {:?}, fd = {}, error = {:?}",
                            ProcessManager::current_pcb().pid(),
                            i,
                            r
                        );
                    }
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct FileDescriptorIterator<'a> {
    fds: &'a FileDescriptorVec,
    index: usize,
}

impl<'a> FileDescriptorIterator<'a> {
    pub fn new(fds: &'a FileDescriptorVec) -> Self {
        return Self { fds, index: 0 };
    }
}

impl<'a> Iterator for FileDescriptorIterator<'a> {
    type Item = (i32, Arc<SpinLock<File>>);

    fn next(&mut self) -> Option<Self::Item> {
        while self.index < FileDescriptorVec::PROCESS_MAX_FD {
            let fd = self.index as i32;
            self.index += 1;
            if let Some(file) = self.fds.get_file_by_fd(fd) {
                return Some((fd, file));
            }
        }
        return None;
    }
}
