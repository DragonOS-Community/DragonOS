use alloc::{boxed::Box, sync::Arc, vec::Vec};

use crate::{
    arch::asm::current::current_pcb,
    filesystem::vfs::file::FileDescriptorVec,
    include::bindings::bindings::{verify_area, AT_REMOVEDIR, PAGE_4K_SIZE, PROC_MAX_FD_NUM},
    io::SeekFrom,
    kerror,
    syscall::{Syscall, SystemError},
};

use super::{
    core::{do_mkdir, do_remove_dir, do_unlink_at},
    file::{File, FileMode},
    utils::rsplit_path,
    Dirent, FileType, IndexNode, ROOT_INODE,
};
use crate::kdebug;

pub const SEEK_SET: u32 = 0;
pub const SEEK_CUR: u32 = 1;
pub const SEEK_END: u32 = 2;
pub const SEEK_MAX: u32 = 3;

impl Syscall {
    /// @brief 为当前进程打开一个文件
    ///
    /// @param path 文件路径
    /// @param o_flags 打开文件的标志位
    ///
    /// @return 文件描述符编号，或者是错误码
    pub fn open(path: &str, mode: FileMode) -> Result<usize, SystemError> {
        // 文件名过长
        if path.len() > PAGE_4K_SIZE as usize {
            return Err(SystemError::ENAMETOOLONG);
        }

        let inode: Result<Arc<dyn IndexNode>, SystemError> = ROOT_INODE().lookup(path);

        let inode: Arc<dyn IndexNode> = if inode.is_err() {
            let errno = inode.unwrap_err();
            // 文件不存在，且需要创建
            if mode.contains(FileMode::O_CREAT)
                && !mode.contains(FileMode::O_DIRECTORY)
                && errno == SystemError::ENOENT
            {
                let (filename, parent_path) = rsplit_path(path);
                // 查找父目录
                let parent_inode: Arc<dyn IndexNode> =
                    ROOT_INODE().lookup(parent_path.unwrap_or("/"))?;
                // 创建文件
                let inode: Arc<dyn IndexNode> =
                    parent_inode.create(filename, FileType::File, 0o777)?;
                inode
            } else {
                // 不需要创建文件，因此返回错误码
                return Err(errno);
            }
        } else {
            inode.unwrap()
        };

        let file_type: FileType = inode.metadata()?.file_type;
        // 如果要打开的是文件夹，而目标不是文件夹
        if mode.contains(FileMode::O_DIRECTORY) && file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        // 如果O_TRUNC，并且，打开模式包含O_RDWR或O_WRONLY，清空文件
        if mode.contains(FileMode::O_TRUNC)
            && (mode.contains(FileMode::O_RDWR) || mode.contains(FileMode::O_WRONLY))
            && file_type == FileType::File
        {
            inode.truncate(0)?;
        }

        // 创建文件对象
        let mut file: File = File::new(inode, mode)?;

        // 打开模式为“追加”
        if mode.contains(FileMode::O_APPEND) {
            file.lseek(SeekFrom::SeekEnd(0))?;
        }
        // 把文件对象存入pcb
        return current_pcb().alloc_fd(file, None).map(|fd| fd as usize);
    }

    /// @brief 关闭文件
    ///
    /// @param fd 文件描述符编号
    ///
    /// @return 成功返回0，失败返回错误码
    pub fn close(fd: usize) -> Result<usize, SystemError> {
        return current_pcb().drop_fd(fd as i32).map(|_| 0);
    }

    /// @brief 发送命令到文件描述符对应的设备，
    ///
    /// @param fd 文件描述符编号
    /// @param cmd 设备相关的请求类型
    ///
    /// @return Ok(usize) 成功返回0
    /// @return Err(SystemError) 读取失败，返回posix错误码
    pub fn ioctl(fd: usize, cmd: u32, data: usize) -> Result<usize, SystemError> {
        let file: Option<&mut File> = current_pcb().get_file_mut_by_fd(fd as i32);
        if file.is_none() {
            return Err(SystemError::EBADF);
        }
        let file: &mut File = file.unwrap();
        file.inode().ioctl(cmd, data)
    }

    /// @brief 根据文件描述符，读取文件数据。尝试读取的数据长度与buf的长度相同。
    ///
    /// @param fd 文件描述符编号
    /// @param buf 输出缓冲区。
    ///
    /// @return Ok(usize) 成功读取的数据的字节数
    /// @return Err(SystemError) 读取失败，返回posix错误码
    pub fn read(fd: i32, buf: &mut [u8]) -> Result<usize, SystemError> {
        let file: Option<&mut File> = current_pcb().get_file_mut_by_fd(fd);
        if file.is_none() {
            return Err(SystemError::EBADF);
        }
        let file: &mut File = file.unwrap();

        return file.read(buf.len(), buf);
    }

    /// @brief 根据文件描述符，向文件写入数据。尝试写入的数据长度与buf的长度相同。
    ///
    /// @param fd 文件描述符编号
    /// @param buf 输入缓冲区。
    ///
    /// @return Ok(usize) 成功写入的数据的字节数
    /// @return Err(SystemError) 写入失败，返回posix错误码
    pub fn write(fd: i32, buf: &[u8]) -> Result<usize, SystemError> {
        let file: Option<&mut File> = current_pcb().get_file_mut_by_fd(fd);
        if file.is_none() {
            return Err(SystemError::EBADF);
        }
        let file: &mut File = file.unwrap();

        return file.write(buf.len(), buf);
    }

    /// @brief 调整文件操作指针的位置
    ///
    /// @param fd 文件描述符编号
    /// @param seek 调整的方式
    ///
    /// @return Ok(usize) 调整后，文件访问指针相对于文件头部的偏移量
    /// @return Err(SystemError) 调整失败，返回posix错误码
    pub fn lseek(fd: i32, seek: SeekFrom) -> Result<usize, SystemError> {
        let file: Option<&mut File> = current_pcb().get_file_mut_by_fd(fd);
        if file.is_none() {
            return Err(SystemError::EBADF);
        }
        let file: &mut File = file.unwrap();
        return file.lseek(seek);
    }

    /// @brief 切换工作目录
    ///
    /// @param dest_path 目标路径
    ///
    /// @return   返回码  描述  
    ///      0       |          成功  
    ///         
    ///   EACCESS    |        权限不足        
    ///
    ///    ELOOP     | 解析path时遇到路径循环
    ///
    /// ENAMETOOLONG |       路径名过长       
    ///
    ///    ENOENT    |  目标文件或目录不存在  
    ///
    ///    ENODIR    |  检索期间发现非目录项  
    ///
    ///    ENOMEM    |      系统内存不足      
    ///
    ///    EFAULT    |       错误的地址      
    ///  
    /// ENAMETOOLONG |        路径过长        
    pub fn chdir(dest_path: &str) -> Result<usize, SystemError> {
        // Copy path to kernel space to avoid some security issues
        let path: Box<&str> = Box::new(dest_path);
        let inode = match ROOT_INODE().lookup(&path) {
            Err(e) => {
                kerror!("Change Directory Failed, Error = {:?}", e);
                return Err(SystemError::ENOENT);
            }
            Ok(i) => i,
        };

        match inode.metadata() {
            Err(e) => {
                kerror!("INode Get MetaData Failed, Error = {:?}", e);
                return Err(SystemError::ENOENT);
            }
            Ok(i) => {
                if let FileType::Dir = i.file_type {
                    return Ok(0);
                } else {
                    return Err(SystemError::ENOTDIR);
                }
            }
        }
    }

    /// @brief 获取目录中的数据
    ///
    /// TODO: 这个函数的语义与Linux不一致，需要修改！！！
    ///
    /// @param fd 文件描述符号
    /// @param buf 输出缓冲区
    ///
    /// @return 成功返回读取的字节数，失败返回错误码
    pub fn getdents(fd: i32, buf: &mut [u8]) -> Result<usize, SystemError> {
        let dirent =
            unsafe { (buf.as_mut_ptr() as *mut Dirent).as_mut() }.ok_or(SystemError::EFAULT)?;

        if fd < 0 || fd as u32 > PROC_MAX_FD_NUM {
            return Err(SystemError::EBADF);
        }

        // 获取fd
        let file: &mut File = match current_pcb().get_file_mut_by_fd(fd) {
            None => {
                return Err(SystemError::EBADF);
            }
            Some(file) => file,
        };
        // kdebug!("file={file:?}");

        return file.readdir(dirent).map(|x| x as usize);
    }

    /// @brief 创建文件夹
    ///
    /// @param path(r8) 路径 / mode(r9) 模式
    ///
    /// @return uint64_t 负数错误码 / 0表示成功
    pub fn mkdir(path: &str, mode: usize) -> Result<usize, SystemError> {
        return do_mkdir(path, FileMode::from_bits_truncate(mode as u32)).map(|x| x as usize);
    }

    /// **删除文件夹、取消文件的链接、删除文件的系统调用**
    ///
    /// ## 参数
    ///
    /// - `dirfd`：文件夹的文件描述符.目前暂未实现
    /// - `pathname`：文件夹的路径
    /// - `flags`：标志位
    ///
    ///
    pub fn unlinkat(_dirfd: i32, pathname: &str, flags: u32) -> Result<usize, SystemError> {
        // kdebug!("sys_unlink_at={path:?}");
        if (flags & (!AT_REMOVEDIR)) != 0 {
            return Err(SystemError::EINVAL);
        }

        if (flags & AT_REMOVEDIR) > 0 {
            // kdebug!("rmdir");
            match do_remove_dir(&pathname) {
                Err(err) => {
                    kerror!("Failed to Remove Directory, Error Code = {:?}", err);
                    return Err(err);
                }
                Ok(_) => {
                    return Ok(0);
                }
            }
        }

        match do_unlink_at(&pathname, FileMode::from_bits_truncate(flags as u32)) {
            Err(err) => {
                kerror!("Failed to Remove Directory, Error Code = {:?}", err);
                return Err(err);
            }
            Ok(_) => {
                return Ok(0);
            }
        }
    }

    /// @brief 根据提供的文件描述符的fd，复制对应的文件结构体，并返回新复制的文件结构体对应的fd
    pub fn dup(oldfd: i32) -> Result<usize, SystemError> {
        if let Some(fds) = FileDescriptorVec::from_pcb(current_pcb()) {
            // 获得当前文件描述符数组
            // 确认oldfd是否有效
            if FileDescriptorVec::validate_fd(oldfd) {
                if let Some(file) = &fds.fds[oldfd as usize] {
                    // 尝试获取对应的文件结构体
                    let file_cp: Box<File> = file.try_clone().ok_or(SystemError::EBADF)?;

                    // 申请文件描述符，并把文件对象存入其中
                    let res = current_pcb().alloc_fd(*file_cp, None).map(|x| x as usize);
                    return res;
                }
                // oldfd对应的文件不存在
                return Err(SystemError::EBADF);
            }
            return Err(SystemError::EBADF);
        } else {
            return Err(SystemError::EMFILE);
        }
    }

    /// 根据提供的文件描述符的fd，和指定新fd，复制对应的文件结构体，
    /// 并返回新复制的文件结构体对应的fd.
    /// 如果新fd已经打开，则会先关闭新fd.
    ///
    /// ## 参数
    ///
    /// - `oldfd`：旧文件描述符
    /// - `newfd`：新文件描述符
    ///
    /// ## 返回值
    ///
    /// - 成功：新文件描述符
    /// - 失败：错误码
    pub fn dup2(oldfd: i32, newfd: i32) -> Result<usize, SystemError> {
        if let Some(fds) = FileDescriptorVec::from_pcb(current_pcb()) {
            // 获得当前文件描述符数组
            if FileDescriptorVec::validate_fd(oldfd) && FileDescriptorVec::validate_fd(newfd) {
                //确认oldfd, newid是否有效
                if oldfd == newfd {
                    // 若oldfd与newfd相等
                    return Ok(newfd as usize);
                }

                if let Some(file) = &fds.fds[oldfd as usize] {
                    if fds.fds[newfd as usize].is_some() {
                        // close newfd
                        if let Err(_) = current_pcb().drop_fd(newfd) {
                            // An I/O error occurred while attempting to close fildes2.
                            return Err(SystemError::EIO);
                        }
                    }

                    // 尝试获取对应的文件结构体
                    let file_cp = file.try_clone();
                    if file_cp.is_none() {
                        return Err(SystemError::EBADF);
                    }
                    // 申请文件描述符，并把文件对象存入其中
                    let res = current_pcb()
                        .alloc_fd(*file_cp.unwrap(), Some(newfd))
                        .map(|x| x as usize);

                    return res;
                }
                return Err(SystemError::EBADF);
            } else {
                return Err(SystemError::EBADF);
            }
        }
        // 从pcb获取文件描述符数组失败
        return Err(SystemError::EMFILE);
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct IoVec {
    /// 缓冲区的起始地址
    pub iov_base: *mut u8,
    /// 缓冲区的长度
    pub iov_len: usize,
}

/// 用于存储多个来自用户空间的IoVec
///
/// 由于目前内核中的文件系统还不支持分散读写，所以暂时只支持将用户空间的IoVec聚合成一个缓冲区，然后进行操作。
/// TODO：支持分散读写
#[derive(Debug)]
pub struct IoVecs(Vec<&'static mut [u8]>);

impl IoVecs {
    /// 从用户空间的IoVec中构造IoVecs
    ///
    /// @param iov 用户空间的IoVec
    /// @param iovcnt 用户空间的IoVec的数量
    /// @param readv 是否为readv系统调用
    ///
    /// @return 构造成功返回IoVecs，否则返回错误码
    pub unsafe fn from_user(
        iov: *const IoVec,
        iovcnt: usize,
        _readv: bool,
    ) -> Result<Self, SystemError> {
        // 检查iov指针所在空间是否合法
        if !verify_area(
            iov as usize as u64,
            (iovcnt * core::mem::size_of::<IoVec>()) as u64,
        ) {
            return Err(SystemError::EFAULT);
        }

        // 将用户空间的IoVec转换为引用（注意：这里的引用是静态的，因为用户空间的IoVec不会被释放）
        let iovs: &[IoVec] = core::slice::from_raw_parts(iov, iovcnt);

        let mut slices: Vec<&mut [u8]> = vec![];
        slices.reserve(iovs.len());

        for iov in iovs.iter() {
            if iov.iov_len == 0 {
                continue;
            }

            if !verify_area(iov.iov_base as usize as u64, iov.iov_len as u64) {
                return Err(SystemError::EFAULT);
            }

            slices.push(core::slice::from_raw_parts_mut(iov.iov_base, iov.iov_len));
        }

        return Ok(Self(slices));
    }

    /// @brief 将IoVecs中的数据聚合到一个缓冲区中
    ///
    /// @return 返回聚合后的缓冲区
    pub fn gather(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        for slice in self.0.iter() {
            buf.extend_from_slice(slice);
        }
        return buf;
    }

    /// @brief 将给定的数据分散写入到IoVecs中
    pub fn scatter(&mut self, data: &[u8]) {
        let mut data: &[u8] = data;
        for slice in self.0.iter_mut() {
            let len = core::cmp::min(slice.len(), data.len());
            if len == 0 {
                continue;
            }

            slice[..len].copy_from_slice(&data[..len]);
            data = &data[len..];
        }
    }

    /// @brief 创建与IoVecs等长的缓冲区
    ///
    /// @param set_len 是否设置返回的Vec的len。
    /// 如果为true，则返回的Vec的len为所有IoVec的长度之和;
    /// 否则返回的Vec的len为0，capacity为所有IoVec的长度之和.
    ///
    /// @return 返回创建的缓冲区
    pub fn new_buf(&self, set_len: bool) -> Vec<u8> {
        let total_len: usize = self.0.iter().map(|slice| slice.len()).sum();
        let mut buf: Vec<u8> = Vec::with_capacity(total_len);

        if set_len {
            unsafe {
                buf.set_len(total_len);
            }
        }
        return buf;
    }
}
