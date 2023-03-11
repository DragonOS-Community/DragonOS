use core::mem::MaybeUninit;

use alloc::{boxed::Box, string::String, sync::Arc};

use crate::{
    arch::asm::current::current_pcb,
    filesystem::procfs::ProcfsFilePrivateData,
    include::bindings::bindings::{
        process_control_block, EINVAL, ENOBUFS, EOVERFLOW, EPERM, ESPIPE,
    },
    io::SeekFrom,
    kerror,
};

use super::{FileType, IndexNode, Metadata};

/// 文件私有信息的枚举类型
#[derive(Debug, Clone)]
pub enum FilePrivateData {
    // procfs文件私有信息
    Procfs(ProcfsFilePrivateData),
    // 不需要文件私有信息
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
    const O_RDONLY = 0;
    /// Open Write-only
    const O_WRONLY = 1;
    /// Open read/write
    const O_RDWR = 2;
    /// Mask for file access modes
    const O_ACCMODE = 00000003;

    /* Bits OR'd into the second argument to open.  */
    /// Create file if it does not exist
    const O_CREAT = 00000100;
    /// Fail if file already exists
    const O_EXCL = 00000200;
    /// Do not assign controlling terminal
    const O_NOCTTY = 00000400;
    /// 文件存在且是普通文件，并以O_RDWR或O_WRONLY打开，则它会被清空
    const O_TRUNC = 00001000;
    /// 文件指针会被移动到文件末尾
    const O_APPEND = 00002000;
    /// 非阻塞式IO模式
    const O_NONBLOCK = 00004000;
    /// used to be O_SYNC, see below
    const O_DSYNC = 00010000;
    /// fcntl, for BSD compatibility
    const FASYNC = 00020000;
    /* direct disk access hint */
    const O_DIRECT = 00040000;
    const O_LARGEFILE = 00100000;
    /// 打开的必须是一个目录
    const O_DIRECTORY = 00200000;
    /// Do not follow symbolic links
    const O_NOFOLLOW = 00400000;
    const O_NOATIME = 01000000;
    /// set close_on_exec
    const O_CLOEXEC = 02000000;
    }
}

/// @brief 抽象文件结构体
#[derive(Debug, Clone)]
pub struct File {
    inode: Arc<dyn IndexNode>,
    offset: usize,
    /// 文件的打开模式
    mode: FileMode,
    /// 文件类型
    file_type: FileType,
    private_data: FilePrivateData,
}

impl File {
    /// @brief 创建一个新的文件对象
    ///
    /// @param inode 文件对象对应的inode
    /// @param mode 文件的打开模式
    pub fn new(inode: Arc<dyn IndexNode>, mode: FileMode) -> Result<Self, i32> {
        let file_type: FileType = inode.metadata()?.file_type;
        let mut f = File {
            inode,
            offset: 0,
            mode,
            file_type,
            private_data: FilePrivateData::default(),
        };
        // kdebug!("inode:{:?}",f.inode);
        f.inode.open(&mut f.private_data)?;
        return Ok(f);
    }

    /// @brief 从文件中读取指定的字节数到buffer中
    ///
    /// @param len 要读取的字节数
    /// @param buf 目标buffer
    ///
    /// @return Ok(usize) 成功读取的字节数
    /// @return Err(i32) 错误码
    pub fn read(&mut self, len: usize, buf: &mut [u8]) -> Result<usize, i32> {
        // 先检查本文件在权限等规则下，是否可读取。
        self.readable()?;

        if buf.len() < len {
            return Err(-(ENOBUFS as i32));
        }

        let len = self
            .inode
            .read_at(self.offset, len, buf, &mut self.private_data)?;

        return Ok(len);
    }

    /// @brief 从buffer向文件写入指定的字节数的数据
    ///
    /// @param len 要写入的字节数
    /// @param buf 源数据buffer
    ///
    /// @return Ok(usize) 成功写入的字节数
    /// @return Err(i32) 错误码
    pub fn write(&mut self, len: usize, buf: &[u8]) -> Result<usize, i32> {
        // 先检查本文件在权限等规则下，是否可写入。
        self.writeable()?;
        if buf.len() < len {
            return Err(-(ENOBUFS as i32));
        }
        let len = self
            .inode
            .write_at(self.offset, len, buf, &mut FilePrivateData::Unused)?;
        return Ok(len);
    }

    /// @brief 获取文件的元数据
    pub fn metadata(&self) -> Result<Metadata, i32> {
        return self.inode.metadata();
    }

    /// @brief 根据inode号获取子目录项的名字
    pub fn get_entry_name(&self, ino: usize) -> Result<String, i32> {
        return self.inode.get_entry_name(ino);
    }

    /// @brief 调整文件操作指针的位置
    ///
    /// @param origin 调整的起始位置
    pub fn lseek(&mut self, origin: SeekFrom) -> Result<usize, i32> {
        if self.inode.metadata().unwrap().file_type == FileType::Pipe {
            return Err(-(ESPIPE as i32));
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
                return Err(-(EINVAL as i32));
            }
        }

        if pos < 0 || pos > self.metadata()?.size {
            return Err(-(EOVERFLOW as i32));
        }
        self.offset = pos as usize;
        return Ok(self.offset);
    }

    /// @brief 判断当前文件是否可读
    #[inline]
    pub fn readable(&self) -> Result<(), i32> {
        // 暂时认为只要不是write only, 就可读
        if self.mode == FileMode::O_WRONLY {
            return Err(-(EPERM as i32));
        }

        return Ok(());
    }

    /// @brief 判断当前文件是否可写
    #[inline]
    pub fn writeable(&self) -> Result<(), i32> {
        // 暂时认为只要不是read only, 就可写
        if self.mode == FileMode::O_RDONLY {
            return Err(-(EPERM as i32));
        }

        return Ok(());
    }


    pub fn inode(&self) -> Arc<dyn IndexNode> {
        return self.inode.clone();
    }
}

impl Drop for File {
    fn drop(&mut self) {
        let r: Result<(), i32> = self.inode.close(&mut self.private_data);
        // 打印错误信息
        if r.is_err() {
            kerror!(
                "pid: {} failed to close file: {:?}, errno={}",
                current_pcb().pid,
                self,
                r.unwrap_err()
            );
        }
    }
}

/// @brief pcb里面的文件描述符数组
#[derive(Debug, Clone)]
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
