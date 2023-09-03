use alloc::{
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};

use crate::{
    filesystem::vfs::file::FileDescriptorVec,
    filesystem::vfs::io::SeekFrom,
    include::bindings::bindings::{verify_area, AT_REMOVEDIR, PAGE_4K_SIZE, PROC_MAX_FD_NUM},
    kerror,
    libs::rwlock::RwLockWriteGuard,
    mm::VirtAddr,
    process::ProcessManager,
    syscall::{Syscall, SystemError},
    time::TimeSpec,
};

use super::{
    core::{do_mkdir, do_remove_dir, do_unlink_at},
    fcntl::{FcntlCommand, FD_CLOEXEC},
    file::{File, FileMode},
    utils::rsplit_path,
    Dirent, FileType, IndexNode, ROOT_INODE,
};

pub const SEEK_SET: u32 = 0;
pub const SEEK_CUR: u32 = 1;
pub const SEEK_END: u32 = 2;
pub const SEEK_MAX: u32 = 3;

bitflags! {
    /// 文件类型和权限
    pub struct ModeType: u32 {
        /// 掩码
        const S_IFMT = 0o0_170_000;
        /// 文件类型
        const S_IFSOCK = 0o140000;
        const S_IFLNK = 0o120000;
        const S_IFREG = 0o100000;
        const S_IFBLK = 0o060000;
        const S_IFDIR = 0o040000;
        const S_IFCHR = 0o020000;
        const S_IFIFO = 0o010000;

        const S_ISUID = 0o004000;
        const S_ISGID = 0o002000;
        const S_ISVTX = 0o001000;
        /// 文件用户权限
        const S_IRWXU = 0o0700;
        const S_IRUSR = 0o0400;
        const S_IWUSR = 0o0200;
        const S_IXUSR = 0o0100;
        /// 文件组权限
        const S_IRWXG = 0o0070;
        const S_IRGRP = 0o0040;
        const S_IWGRP = 0o0020;
        const S_IXGRP = 0o0010;
        /// 文件其他用户权限
        const S_IRWXO = 0o0007;
        const S_IROTH = 0o0004;
        const S_IWOTH = 0o0002;
        const S_IXOTH = 0o0001;
    }
}

#[repr(C)]
/// # 文件信息结构体
pub struct PosixKstat {
    /// 硬件设备ID
    dev_id: u64,
    /// inode号
    inode: u64,
    /// 硬链接数
    nlink: u64,
    /// 文件权限
    mode: ModeType,
    /// 所有者用户ID
    uid: i32,
    /// 所有者组ID
    gid: i32,
    /// 设备ID
    rdev: i64,
    /// 文件大小
    size: i64,
    /// 文件系统块大小
    blcok_size: i64,
    /// 分配的512B块数
    blocks: u64,
    /// 最后访问时间
    atime: TimeSpec,
    /// 最后修改时间
    mtime: TimeSpec,
    /// 最后状态变化时间
    ctime: TimeSpec,
    /// 用于填充结构体大小的空白数据
    pub _pad: [i8; 24],
}
impl PosixKstat {
    fn new() -> Self {
        Self {
            inode: 0,
            dev_id: 0,
            mode: ModeType { bits: 0 },
            nlink: 0,
            uid: 0,
            gid: 0,
            rdev: 0,
            size: 0,
            atime: TimeSpec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            mtime: TimeSpec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            ctime: TimeSpec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            blcok_size: 0,
            blocks: 0,
            _pad: Default::default(),
        }
    }
}
impl Syscall {
    /// @brief 为当前进程打开一个文件
    ///
    /// @param path 文件路径
    /// @param o_flags 打开文件的标志位
    ///
    /// @return 文件描述符编号，或者是错误码
    pub fn open(path: &str, mode: FileMode) -> Result<usize, SystemError> {
        // kdebug!("open: path: {}, mode: {:?}", path, mode);
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
        return ProcessManager::current_pcb()
            .fd_table()
            .write()
            .alloc_fd(file, None)
            .map(|fd| fd as usize);
    }

    /// @brief 关闭文件
    ///
    /// @param fd 文件描述符编号
    ///
    /// @return 成功返回0，失败返回错误码
    pub fn close(fd: usize) -> Result<usize, SystemError> {
        let binding = ProcessManager::current_pcb().fd_table();
        let mut fd_table_guard = binding.write();

        return fd_table_guard.drop_fd(fd as i32).map(|_| 0);
    }

    /// @brief 根据文件描述符，读取文件数据。尝试读取的数据长度与buf的长度相同。
    ///
    /// @param fd 文件描述符编号
    /// @param buf 输出缓冲区。
    ///
    /// @return Ok(usize) 成功读取的数据的字节数
    /// @return Err(SystemError) 读取失败，返回posix错误码
    pub fn read(fd: i32, buf: &mut [u8]) -> Result<usize, SystemError> {
        let binding = ProcessManager::current_pcb().fd_table();
        let mut fd_table_guard = binding.write();

        let file: Option<&mut File> = fd_table_guard.get_file_mut_by_fd(fd);
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
        let binding = ProcessManager::current_pcb().fd_table();
        let mut fd_table_guard = binding.write();

        let file: Option<&mut File> = fd_table_guard.get_file_mut_by_fd(fd);
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
        let binding = ProcessManager::current_pcb().fd_table();
        let mut fd_table_guard = binding.write();
        let file: Option<&mut File> = fd_table_guard.get_file_mut_by_fd(fd);
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
        let proc = ProcessManager::current_pcb();
        // Copy path to kernel space to avoid some security issues
        let path = dest_path.to_string();
        let mut new_path = String::from("");
        if path.len() > 0 {
            let cwd = match path.as_bytes()[0] {
                b'/' => String::from("/"),
                _ => proc.basic().cwd(),
            };
            let mut cwd_vec: Vec<_> = cwd.split("/").filter(|&x| x != "").collect();
            let path_split = path.split("/").filter(|&x| x != "");
            for seg in path_split {
                if seg == ".." {
                    cwd_vec.pop();
                } else if seg == "." {
                    // 当前目录
                } else {
                    cwd_vec.push(seg);
                }
            }
            //proc.basic().set_path(String::from(""));
            for seg in cwd_vec {
                new_path.push_str("/");
                new_path.push_str(seg);
            }
            if new_path == "" {
                new_path = String::from("/");
            }
        }
        let inode = match ROOT_INODE().lookup(&new_path) {
            Err(e) => {
                kerror!("Change Directory Failed, Error = {:?}", e);
                return Err(SystemError::ENOENT);
            }
            Ok(i) => i,
        };
        let metadata = inode.metadata()?;
        if metadata.file_type == FileType::Dir {
            proc.basic_mut().set_cwd(String::from(new_path));
            return Ok(0);
        } else {
            return Err(SystemError::ENOTDIR);
        }
    }

    /// @brief 获取当前进程的工作目录路径
    ///
    /// @param buf 指向缓冲区的指针
    /// @param size 缓冲区的大小
    ///
    /// @return 成功，返回的指针指向包含工作目录路径的字符串
    /// @return 错误，没有足够的空间
    pub fn getcwd(buf: &mut [u8]) -> Result<VirtAddr, SystemError> {
        let proc = ProcessManager::current_pcb();
        let cwd = proc.basic().cwd();

        let cwd_bytes = cwd.as_bytes();
        let cwd_len = cwd_bytes.len();
        if cwd_len + 1 > buf.len() {
            return Err(SystemError::ENOMEM);
        }
        buf[..cwd_len].copy_from_slice(cwd_bytes);
        buf[cwd_len] = 0;

        return Ok(VirtAddr::new(buf.as_ptr() as usize));
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
        let binding = ProcessManager::current_pcb().fd_table();
        let mut fd_table_guard = binding.write();
        let file: &mut File = match fd_table_guard.get_file_mut_by_fd(fd) {
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
        let binding = ProcessManager::current_pcb().fd_table();
        let mut fd_table_guard = binding.write();

        let old_file = fd_table_guard
            .get_file_ref_by_fd(oldfd)
            .ok_or(SystemError::EBADF)?;

        let new_file = old_file.try_clone().ok_or(SystemError::EBADF)?;
        // 申请文件描述符，并把文件对象存入其中
        let res = fd_table_guard.alloc_fd(new_file, None).map(|x| x as usize);
        return res;
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
        let binding = ProcessManager::current_pcb().fd_table();
        let mut fd_table_guard = binding.write();
        return Self::do_dup2(oldfd, newfd, &mut fd_table_guard);
    }

    fn do_dup2(
        oldfd: i32,
        newfd: i32,
        fd_table_guard: &mut RwLockWriteGuard<'_, FileDescriptorVec>,
    ) -> Result<usize, SystemError> {
        // 确认oldfd, newid是否有效
        if !(FileDescriptorVec::validate_fd(oldfd) && FileDescriptorVec::validate_fd(newfd)) {
            return Err(SystemError::EBADF);
        }

        if oldfd == newfd {
            // 若oldfd与newfd相等
            return Ok(newfd as usize);
        }
        let new_exists = fd_table_guard.get_file_ref_by_fd(newfd).is_some();
        if new_exists {
            // close newfd
            if let Err(_) = fd_table_guard.drop_fd(newfd) {
                // An I/O error occurred while attempting to close fildes2.
                return Err(SystemError::EIO);
            }
        }

        let old_file = fd_table_guard
            .get_file_ref_by_fd(oldfd)
            .ok_or(SystemError::EBADF)?;
        let new_file = old_file.try_clone().ok_or(SystemError::EBADF)?;
        // 申请文件描述符，并把文件对象存入其中
        let res = fd_table_guard
            .alloc_fd(new_file, Some(newfd))
            .map(|x| x as usize);
        return res;
    }

    /// # fcntl
    ///
    /// ## 参数
    ///
    /// - `fd`：文件描述符
    /// - `cmd`：命令
    /// - `arg`：参数
    pub fn fcntl(fd: i32, cmd: FcntlCommand, arg: i32) -> Result<usize, SystemError> {
        match cmd {
            FcntlCommand::DupFd => {
                if arg < 0 || arg as usize >= FileDescriptorVec::PROCESS_MAX_FD {
                    return Err(SystemError::EBADF);
                }
                let arg = arg as usize;
                for i in arg..FileDescriptorVec::PROCESS_MAX_FD {
                    let binding = ProcessManager::current_pcb().fd_table();
                    let mut fd_table_guard = binding.write();
                    if fd_table_guard.get_file_ref_by_fd(fd).is_none() {
                        return Self::do_dup2(fd, i as i32, &mut fd_table_guard);
                    }
                }
                return Err(SystemError::EMFILE);
            }
            FcntlCommand::GetFd => {
                // Get file descriptor flags.
                let binding = ProcessManager::current_pcb().fd_table();
                let fd_table_guard = binding.read();
                if let Some(file) = fd_table_guard.get_file_ref_by_fd(fd) {
                    if file.close_on_exec() {
                        return Ok(FD_CLOEXEC as usize);
                    }
                }
                return Err(SystemError::EBADF);
            }
            FcntlCommand::SetFd => {
                // Set file descriptor flags.
                let binding = ProcessManager::current_pcb().fd_table();
                let mut fd_table_guard = binding.write();

                if let Some(file) = fd_table_guard.get_file_mut_by_fd(fd) {
                    let arg = arg as u32;
                    if arg & FD_CLOEXEC != 0 {
                        file.set_close_on_exec(true);
                    } else {
                        file.set_close_on_exec(false);
                    }
                    return Ok(0);
                }
                return Err(SystemError::EBADF);
            }

            FcntlCommand::GetFlags => {
                // Get file status flags.
                let binding = ProcessManager::current_pcb().fd_table();
                let fd_table_guard = binding.read();

                if let Some(file) = fd_table_guard.get_file_ref_by_fd(fd) {
                    return Ok(file.mode().bits() as usize);
                }

                return Err(SystemError::EBADF);
            }
            FcntlCommand::SetFlags => {
                // Set file status flags.
                let binding = ProcessManager::current_pcb().fd_table();
                let mut fd_table_guard = binding.write();

                if let Some(file) = fd_table_guard.get_file_mut_by_fd(fd) {
                    let arg = arg as u32;
                    let mode = FileMode::from_bits(arg).ok_or(SystemError::EINVAL)?;
                    file.set_mode(mode)?;
                    return Ok(0);
                }

                return Err(SystemError::EBADF);
            }
            _ => {
                // TODO: unimplemented
                // 未实现的命令，返回0，不报错。

                // kwarn!("fcntl: unimplemented command: {:?}, defaults to 0.", cmd);
                return Ok(0);
            }
        }
    }

    /// # ftruncate
    ///
    /// ## 描述
    ///
    /// 改变文件大小.
    /// 如果文件大小大于原来的大小，那么文件的内容将会被扩展到指定的大小，新的空间将会用0填充.
    /// 如果文件大小小于原来的大小，那么文件的内容将会被截断到指定的大小.
    ///
    /// ## 参数
    ///
    /// - `fd`：文件描述符
    /// - `len`：文件大小
    ///
    /// ## 返回值
    ///
    /// 如果成功，返回0，否则返回错误码.
    pub fn ftruncate(fd: i32, len: usize) -> Result<usize, SystemError> {
        let binding = ProcessManager::current_pcb().fd_table();
        let fd_table_guard = binding.read();

        if let Some(file) = fd_table_guard.get_file_ref_by_fd(fd) {
            let r = file.ftruncate(len).map(|_| 0);
            return r;
        }

        return Err(SystemError::EBADF);
    }
    fn do_fstat(fd: i32) -> Result<PosixKstat, SystemError> {
        let binding = ProcessManager::current_pcb().fd_table();
        let fd_table_guard = binding.read();
        match fd_table_guard.get_file_ref_by_fd(fd) {
            Some(file) => {
                let mut kstat = PosixKstat::new();
                // 获取文件信息
                match file.metadata() {
                    Ok(metadata) => {
                        kstat.size = metadata.size as i64;
                        kstat.dev_id = metadata.dev_id as u64;
                        kstat.inode = metadata.inode_id as u64;
                        kstat.blcok_size = metadata.blk_size as i64;
                        kstat.blocks = metadata.blocks as u64;

                        kstat.atime.tv_sec = metadata.atime.tv_sec;
                        kstat.atime.tv_nsec = metadata.atime.tv_nsec;
                        kstat.mtime.tv_sec = metadata.mtime.tv_sec;
                        kstat.mtime.tv_nsec = metadata.mtime.tv_nsec;
                        kstat.ctime.tv_sec = metadata.ctime.tv_sec;
                        kstat.ctime.tv_nsec = metadata.ctime.tv_nsec;

                        kstat.nlink = metadata.nlinks as u64;
                        kstat.uid = metadata.uid as i32;
                        kstat.gid = metadata.gid as i32;
                        kstat.rdev = metadata.raw_dev as i64;
                        kstat.mode.bits = metadata.mode;
                        match file.file_type() {
                            FileType::File => kstat.mode.insert(ModeType::S_IFMT),
                            FileType::Dir => kstat.mode.insert(ModeType::S_IFDIR),
                            FileType::BlockDevice => kstat.mode.insert(ModeType::S_IFBLK),
                            FileType::CharDevice => kstat.mode.insert(ModeType::S_IFCHR),
                            FileType::SymLink => kstat.mode.insert(ModeType::S_IFLNK),
                            FileType::Socket => kstat.mode.insert(ModeType::S_IFSOCK),
                            FileType::Pipe => kstat.mode.insert(ModeType::S_IFIFO),
                        }
                    }
                    Err(e) => return Err(e),
                }

                return Ok(kstat);
            }
            None => {
                return Err(SystemError::EINVAL);
            }
        }
    }
    pub fn fstat(fd: i32, usr_kstat: *mut PosixKstat) -> Result<usize, SystemError> {
        match Self::do_fstat(fd) {
            Ok(kstat) => {
                if usr_kstat.is_null() {
                    return Err(SystemError::EFAULT);
                }
                unsafe {
                    *usr_kstat = kstat;
                }
                return Ok(0);
            }
            Err(e) => return Err(e),
        }
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
