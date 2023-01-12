#![allow(dead_code)]

pub mod file;

use core::any::Any;

use alloc::{string::String, sync::Arc};

use crate::{include::bindings::bindings::ENOTSUP, time::TimeSpec};

/// vfs容许的最大的路径名称长度
pub const MAX_PATHLEN: u32 = 1024;

/// 文件的类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    File,
    Dir,
    BlockDevice,
    CharDevice,
    Pipe
}

/// @brief inode的状态（由poll方法返回）
#[derive(Debug, Default, PartialEq)]
pub struct PollStatus {
    pub flags: u8,
}

impl PollStatus {
    pub const WRITE_MASK: u8 = (1u8 << 0);
    pub const READ_MASK: u8 = (1u8 << 1);
    pub const ERR_MASK: u8 = (1u8 << 2);
}

pub trait IndexNode: Any + Sync + Send {
    /// @brief 在inode的指定偏移量开始，读取指定大小的数据
    ///
    /// @param offset 起始位置在Inode中的偏移量
    /// @param len 要读取的字节数
    /// @param buf 缓冲区. 请注意，必须满足@buf.len()>=@len
    ///
    /// @return 成功：Ok(读取的字节数)
    ///         失败：Err(Posix错误码)
    fn read_at(&self, offset: usize, len: usize, buf: &mut [u8]) -> Result<usize, i32>;

    /// @brief 在inode的指定偏移量开始，写入指定大小的数据（从buf的第0byte开始写入）
    ///
    /// @param offset 起始位置在Inode中的偏移量
    /// @param len 要写入的字节数
    /// @param buf 缓冲区. 请注意，必须满足@buf.len()>=@len
    ///
    /// @return 成功：Ok(写入的字节数)
    ///         失败：Err(Posix错误码)
    fn write_at(&self, offset: usize, len: usize, buf: &mut [u8]) -> Result<usize, i32>;

    /// @brief 获取当前inode的状态。
    ///
    /// @return PollStatus结构体
    fn poll(&self) -> PollStatus;

    /// @brief 获取inode的元数据
    ///
    /// @return 成功：Ok(inode的元数据)
    ///         失败：Err(错误码)
    fn metadata(&self) -> Result<Metadata, i32> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(-(ENOTSUP as i32));
    }

    /// @brief 设置inode的元数据
    ///
    /// @return 成功：Ok()
    ///         失败：Err(错误码)
    fn set_metadata(&self, _metadata: &Metadata) -> Result<(), i32> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(-(ENOTSUP as i32));
    }

    /// @brief 重新设置文件的大小
    ///
    /// @return 成功：Ok()
    ///         失败：Err(错误码)
    fn resize(&self, _len: usize) -> Result<(), i32> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(-(ENOTSUP as i32));
    }

    /// @brief 在当前目录下创建一个新的inode
    ///
    /// @param name 目录项的名字
    /// @param file_type 文件类型
    /// @param mode 权限
    ///
    /// @return 创建成功：返回Ok(新的inode的Arc指针)
    /// @return 创建失败：返回Err(错误码)
    fn create(
        &self,
        name: &str,
        file_type: FileType,
        mode: u32,
    ) -> Result<Arc<dyn IndexNode>, i32> {
        // 若文件系统没有实现此方法，则默认调用其create_with_data方法。如果仍未实现，则会得到一个Err(-ENOTSUP)的返回值
        return self.create_with_data(name, file_type, mode, 0);
    }

    /// @brief 在当前目录下创建一个新的inode，并传入一个简单的data字段，方便进行初始化。
    ///
    /// @param name 目录项的名字
    /// @param file_type 文件类型
    /// @param mode 权限
    /// @param data 用于初始化该inode的数据。（为0则表示忽略此字段）
    ///
    /// @return 创建成功：返回Ok(新的inode的Arc指针)
    /// @return 创建失败：返回Err(错误码)
    fn create_with_data(
        &self,
        _name: &str,
        _file_type: FileType,
        _mode: u32,
        _data: usize,
    ) -> Result<Arc<dyn IndexNode>, i32> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(-(ENOTSUP as i32));
    }

    /// @brief 创建一个名为Name的硬链接，指向另一个IndexNode
    ///
    /// @param name 硬链接的名称
    /// @param other 要被指向的IndexNode的Arc指针
    ///
    /// @return 成功：Ok()
    ///         失败：Err(错误码)
    fn link(&self, _name: &str, _other: &Arc<dyn IndexNode>) -> Result<(), i32> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(-(ENOTSUP as i32));
    }

    /// @brief 删除一个名为Name的硬链接
    ///
    /// @param name 硬链接的名称
    ///
    /// @return 成功：Ok()
    ///         失败：Err(错误码)
    fn unlink(&self, _name: &str) -> Result<(), i32> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(-(ENOTSUP as i32));
    }

    /// @brief 删除一个名为Name的inode
    ///
    /// @param name 要寻找的inode的名称
    ///
    /// @return 成功：Ok()
    ///         失败：Err(错误码)
    fn find(&self, _name: &str) -> Result<Arc<dyn IndexNode>, i32> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(-(ENOTSUP as i32));
    }

    /// @brief 根据inode号，获取子目录项的名字
    ///
    /// @param ino inode号
    ///
    /// @return 成功：Ok()
    ///         失败：Err(错误码)
    fn get_entry_name(&self, _ino: usize) -> Result<String, i32> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(-(ENOTSUP as i32));
    }

    /// @brief 根据inode号，获取目录项的名字
    ///
    /// @param ino inode号
    ///
    /// @return 成功：Ok()
    ///         失败：Err(错误码)
    fn get_entry_name_and_metadata(&self, ino: usize) -> Result<(String, Metadata), i32> {
        // 如果有条件，请在文件系统中使用高效的方式实现本接口，而不是依赖这个低效率的默认实现。
        let name = self.get_entry_name(ino)?;
        let entry = self.find(&name)?;
        return Ok((name, entry.metadata()?));
    }

    /// @brief io control接口
    ///
    /// @param cmd 命令
    /// @param data 数据
    /// 
    /// @return 成功：Ok()
    ///         失败：Err(错误码)
    fn ioctl(&self, _cmd: u32, _data: usize) -> Result<usize, i32> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(-(ENOTSUP as i32));
    }

    /// @brief 获取inode所在的文件系统的指针
    fn fs(&self) -> Arc<dyn FileSystem>;

    /// @brief 本函数用于实现动态转换。
    /// 具体的文件系统在实现本函数时，最简单的方式就是：直接返回self
    fn as_any_ref(&self) -> &dyn Any;
}

impl dyn IndexNode{

}

/// IndexNode的元数据
///
/// 对应Posix2008中的sys/stat.h中的定义 https://pubs.opengroup.org/onlinepubs/9699919799/basedefs/sys_stat.h.html
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Metadata {
    /// 当前inode所在的文件系统的设备号
    pub dev_id: usize,
    /// inode号
    pub inode_id: usize,

    /// Inode的大小
    /// 文件：文件大小（单位：字节）
    /// 目录：目录项中的文件、文件夹数量
    pub size: i64,

    /// Inode所在的文件系统中，每个块的大小
    pub blk_size: usize,

    /// Inode所占的块的数目
    pub blocks: usize,

    /// inode最后一次被访问的时间
    pub atime: TimeSpec,

    /// inode最后一次修改的时间
    pub mtime: TimeSpec,

    /// inode的创建时间
    pub ctime: TimeSpec,

    /// 文件类型
    pub file_type: FileType,

    /// 权限
    pub mode: u32,

    /// 硬链接的数量
    pub nlinks: usize,

    /// User ID
    pub uid: usize,

    /// Group ID
    pub gid: usize,

    /// 文件指向的设备的id（对于设备文件系统来说）
    pub raw_dev: usize,
}

/// @brief 所有文件系统都应该实现的trait
pub trait FileSystem: Sync + Send {
    /// @brief 获取当前文件系统的root inode的指针
    fn get_root_inode(&self) -> Arc<dyn IndexNode>;

    /// @brief 获取当前文件系统的信息
    fn into(&self) -> FsInfo;
}

#[derive(Debug)]
pub struct FsInfo {
    /// 文件系统所在的块设备的id
    blk_dev_id: usize,
    /// 文件名的最大长度
    max_name_len: usize,
}
