use core::mem::MaybeUninit;

use alloc::{boxed::Box, string::String, sync::Arc, vec::Vec};

use crate::{
    arch::asm::current::current_pcb, driver::tty::TtyFilePrivateData,
    filesystem::procfs::ProcfsFilePrivateData, include::bindings::bindings::process_control_block,
    io::SeekFrom, kerror, syscall::SystemError,
};

use super::{Dirent, FileType, IndexNode, Metadata};

/// 文件私有信息的枚举类型
#[derive(Debug, Clone)]
pub enum FilePrivateData {
    /// procfs文件私有信息
    Procfs(ProcfsFilePrivateData),
    /// Tty设备的私有信息
    Tty(TtyFilePrivateData),
    /// 不需要文件私有信息
    Unused,
}

impl Default for FilePrivateData {
    fn default() -> Self {
        return Self::Unused;
    }
}

bitflags! {
    /// @brief 文件打开模式
    /// 其中，低2bit组合而成的数字的值，用于表示访问权限。其他的bit，才支持通过按位或的方式来表示参数
    ///
    /// 与Linux 5.19.10的uapi/asm-generic/fcntl.h相同
    /// https://opengrok.ringotek.cn/xref/linux-5.19.10/tools/include/uapi/asm-generic/fcntl.h#19
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
        let file_type: FileType = inode.metadata()?.file_type;
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
        // 先检查本文件在权限等规则下，是否可读取。
        self.readable()?;

        if buf.len() < len {
            return Err(SystemError::ENOBUFS);
        }
        let len = self
            .inode
            .read_at(self.offset, len, buf, &mut self.private_data)?;
        self.offset += len;
        return Ok(len);
    }

    /// @brief 从buffer向文件写入指定的字节数的数据
    ///
    /// @param len 要写入的字节数
    /// @param buf 源数据buffer
    ///
    /// @return Ok(usize) 成功写入的字节数
    /// @return Err(SystemError) 错误码
    pub fn write(&mut self, len: usize, buf: &[u8]) -> Result<usize, SystemError> {
        // 先检查本文件在权限等规则下，是否可写入。
        self.writeable()?;
        if buf.len() < len {
            return Err(SystemError::ENOBUFS);
        }
        let len = self
            .inode
            .write_at(self.offset, len, buf, &mut self.private_data)?;
        self.offset += len;
        return Ok(len);
    }

    /// @brief 获取文件的元数据
    pub fn metadata(&self) -> Result<Metadata, SystemError> {
        return self.inode.metadata();
    }

    /// @brief 根据inode号获取子目录项的名字
    pub fn get_entry_name(&self, ino: usize) -> Result<String, SystemError> {
        return self.inode.get_entry_name(ino);
    }

    /// @brief 调整文件操作指针的位置
    ///
    /// @param origin 调整的起始位置
    pub fn lseek(&mut self, origin: SeekFrom) -> Result<usize, SystemError> {
        if self.inode.metadata().unwrap().file_type == FileType::Pipe {
            return Err(SystemError::ESPIPE);
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

        if pos < 0 || pos > self.metadata()?.size {
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
            self.readdir_subdirs_name = inode.list()?;
            self.readdir_subdirs_name.sort();
        }

        // kdebug!("sub_entries={sub_entries:?}");
        if self.readdir_subdirs_name.is_empty() {
            self.offset = 0;
            return Ok(0);
        }
        let name: String = self.readdir_subdirs_name.remove(0);
        let sub_inode: Arc<dyn IndexNode> = match inode.find(&name) {
            Ok(i) => i,
            Err(e) => {
                kerror!("Readdir error: Failed to find sub inode, file={self:?}");
                return Err(e);
            }
        };

        let name_bytes: &[u8] = name.as_bytes();

        self.offset += 1;
        dirent.d_ino = sub_inode.metadata().unwrap().inode_id as u64;
        dirent.d_off = 0;
        dirent.d_reclen = 0;
        dirent.d_type = sub_inode.metadata().unwrap().file_type.get_file_type_num() as u8;
        // 根据posix的规定，dirent中的d_name是一个不定长的数组，因此需要unsafe来拷贝数据
        unsafe {
            let ptr = &mut dirent.d_name as *mut u8;
            let buf: &mut [u8] =
                ::core::slice::from_raw_parts_mut::<'static, u8>(ptr, name_bytes.len());
            buf.copy_from_slice(name_bytes);
        }

        // 计算dirent结构体的大小
        return Ok((name_bytes.len() + ::core::mem::size_of::<Dirent>()
            - ::core::mem::size_of_val(&dirent.d_name)) as u64);
    }

    pub fn inode(&self) -> Arc<dyn IndexNode> {
        return self.inode.clone();
    }

    /// @brief 尝试克隆一个文件
    ///
    /// @return Option<Box<File>> 克隆后的文件结构体。如果克隆失败，返回None
    pub fn try_clone(&self) -> Option<Box<File>> {
        let mut res: Box<File> = Box::new(Self {
            inode: self.inode.clone(),
            offset: self.offset.clone(),
            mode: self.mode.clone(),
            file_type: self.file_type.clone(),
            readdir_subdirs_name: self.readdir_subdirs_name.clone(),
            private_data: self.private_data.clone(),
        });
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
}

impl Drop for File {
    fn drop(&mut self) {
        let r: Result<(), SystemError> = self.inode.close(&mut self.private_data);
        // 打印错误信息
        if r.is_err() {
            kerror!(
                "pid: {} failed to close file: {:?}, errno={:?}",
                current_pcb().pid,
                self,
                r.unwrap_err()
            );
        }
    }
}

/// @brief pcb里面的文件描述符数组
#[derive(Debug)]
pub struct FileDescriptorVec {
    /// 当前进程打开的文件描述符
    pub fds: [Option<Box<File>>; FileDescriptorVec::PROCESS_MAX_FD],
}

impl FileDescriptorVec {
    pub const PROCESS_MAX_FD: usize = 32;

    pub fn new() -> Box<FileDescriptorVec> {
        // 先声明一个未初始化的数组
        let mut data: [MaybeUninit<Option<Box<File>>>; FileDescriptorVec::PROCESS_MAX_FD] =
            unsafe { MaybeUninit::uninit().assume_init() };

        // 逐个把每个元素初始化为None
        for i in 0..FileDescriptorVec::PROCESS_MAX_FD {
            data[i] = MaybeUninit::new(None);
        }
        // 由于一切都初始化完毕，因此将未初始化的类型强制转换为已经初始化的类型
        let data: [Option<Box<File>>; FileDescriptorVec::PROCESS_MAX_FD] = unsafe {
            core::mem::transmute::<_, [Option<Box<File>>; FileDescriptorVec::PROCESS_MAX_FD]>(data)
        };

        // 初始化文件描述符数组结构体
        return Box::new(FileDescriptorVec { fds: data });
    }

    /// @brief 克隆一个文件描述符数组
    ///
    /// @return Box<FileDescriptorVec> 克隆后的文件描述符数组
    pub fn clone(&self) -> Box<FileDescriptorVec> {
        let mut res: Box<FileDescriptorVec> = FileDescriptorVec::new();
        for i in 0..FileDescriptorVec::PROCESS_MAX_FD {
            if let Some(file) = &self.fds[i] {
                res.fds[i] = file.try_clone();
            }
        }
        return res;
    }

    /// @brief 从pcb的fds字段，获取文件描述符数组的可变引用
    #[inline]
    pub fn from_pcb(pcb: &'static process_control_block) -> Option<&'static mut FileDescriptorVec> {
        return unsafe { (pcb.fds as usize as *mut FileDescriptorVec).as_mut() };
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
}
