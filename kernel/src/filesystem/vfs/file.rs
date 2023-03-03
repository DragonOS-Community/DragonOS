use alloc::{string::String, sync::Arc, vec::Vec};

use crate::{include::bindings::bindings::{EINVAL, ENOBUFS, EOVERFLOW, EPERM, ESPIPE}, io::SeekFrom, filesystem::procfs::ProcfsFilePrivateData, kdebug};

use super::{FileType, IndexNode, Metadata};

/// 文件私有信息的枚举类型
#[derive(Debug)]
pub enum FilePrivateData {
    // procfs文件私有信息
    Procfs(ProcfsFilePrivateData),
    // 不需要文件私有信息
    Unused
}

impl Default for FilePrivateData{
    fn default() -> Self {
        return Self::Unused;
    }
}

/// @brief 文件打开模式
/// 其中，低2bit组合而成的数字的值，用于表示访问权限。其他的bit，才支持通过按位或的方式来表示参数
#[repr(u32)]
#[derive(PartialEq)]
#[allow(non_camel_case_types)]
pub enum FileMode {
    /* File access modes for `open' and `fcntl'.  */
    /// Open Read-only
    O_RDONLY = 0,
    /// Open Write-only
    O_WRONLY = 1,
    /// Open read/write
    O_RDWR = 2,
    /// Mask for file access modes
    O_ACCMODE = 00000003,

    /* Bits OR'd into the second argument to open.  */
    /// Create file if it does not exist
    O_CREAT = 0000010,
    /// Fail if file already exists
    O_EXCL = 0000020,
    /// Do not assign controlling terminal
    O_NOCTTY = 0000040,
    /// 文件存在且是普通文件，并以O_RDWR或O_WRONLY打开，则它会被清空
    O_TRUNC = 0000100,
    /// 文件指针会被移动到文件末尾
    O_APPEND = 0000200,
    /// 非阻塞式IO模式
    O_NONBLOCK = 0000400,
    /// 以仅执行的方式打开（非目录文件）
    O_EXEC = 0001000,
    /// Open the directory for search only
    O_SEARCH = 0002000,
    /// 打开的必须是一个目录
    O_DIRECTORY = 0004000,
    /// Do not follow symbolic links
    O_NOFOLLOW = 0010000,
}

/// @brief 抽象文件结构体
pub struct File {
    inode: Arc<dyn IndexNode>,
    offset: usize,
    mode: u32,
    private_data: FilePrivateData,
}

impl File {
    /// @brief 创建一个新的文件对象
    ///
    /// @param inode 文件对象对应的inode
    /// @param mode 文件的打开模式
    pub fn new(inode: Arc<dyn IndexNode>, mode: u32) -> Result<Self, i32> {
        let mut f =  File {
            inode,
            offset: 0,
            mode,
            private_data: FilePrivateData::default(),
        };
        // kdebug!("inode:{:?}",f.inode);
        f.inode.open(&mut f.private_data)?;
        kdebug!("File open: f.private_data={:?}", f.private_data);
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

        let len = self.inode.read_at(self.offset, len, buf, &mut self.private_data)?;

        return Ok(len);
    }

    /// @brief 从buffer向文件写入指定的字节数的数据
    ///
    /// @param len 要写入的字节数
    /// @param buf 源数据buffer
    ///
    /// @return Ok(usize) 成功写入的字节数
    /// @return Err(i32) 错误码
    pub fn write(&mut self, len: usize, buf: &mut [u8]) -> Result<usize, i32> {
        // 先检查本文件在权限等规则下，是否可写入。
        self.writeable()?;
        if buf.len() < len {
            return Err(-(ENOBUFS as i32));
        }
        let len = self.inode.write_at(self.offset, len, buf, &mut FilePrivateData::Unused)?;
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
        if self.mode == FileMode::O_WRONLY as u32 {
            return Err(-(EPERM as i32));
        }

        return Ok(());
    }

    /// @brief 判断当前文件是否可写
    #[inline]
    pub fn writeable(&self) -> Result<(), i32> {
        // 暂时认为只要不是read only, 就可写
        if self.mode == FileMode::O_RDONLY as u32 {
            return Err(-(EPERM as i32));
        }

        return Ok(());
    }
}

impl Drop for File{
    fn drop(&mut self) {
        self.inode.close(&mut self.private_data).ok();
    }
}