#![allow(dead_code)]

pub mod core;
pub mod file;
pub mod mount;

use ::core::{any::Any, fmt::Debug};

use alloc::{string::String, sync::Arc, vec::Vec};

use crate::{
    include::bindings::bindings::{ENOTDIR, ENOTSUP},
    time::TimeSpec,
};

/// vfs容许的最大的路径名称长度
pub const MAX_PATHLEN: u32 = 1024;

/// 定义inode号的类型为usize
pub type InodeId = usize;

/// 文件的类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    /// 文件
    File,
    /// 文件夹
    Dir,
    /// 块设备
    BlockDevice,
    /// 字符设备
    CharDevice,
    /// 管道文件
    Pipe,
    /// 符号链接
    SymLink,
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

pub trait IndexNode: Any + Sync + Send + Debug {
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
    fn poll(&self) -> Result<PollStatus, i32>;

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

    /// @brief 在当前目录下，创建一个名为Name的硬链接，指向另一个IndexNode
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

    /// @brief 在当前目录下，删除一个名为Name的硬链接
    ///
    /// @param name 硬链接的名称
    ///
    /// @return 成功：Ok()
    ///         失败：Err(错误码)
    fn unlink(&self, _name: &str) -> Result<(), i32> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(-(ENOTSUP as i32));
    }

    /// @brief 将指定名称的子目录项的文件内容，移动到target这个目录下。如果_old_name所指向的inode与_target的相同，那么则直接执行重命名的操作。
    ///
    /// @param old_name 旧的名字
    fn move_(
        &self,
        _old_name: &str,
        _target: &Arc<dyn IndexNode>,
        _new_name: &str,
    ) -> Result<(), i32> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(-(ENOTSUP as i32));
    }

    /// @brief 寻找一个名为Name的inode
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
    fn get_entry_name(&self, _ino: InodeId) -> Result<String, i32> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(-(ENOTSUP as i32));
    }

    /// @brief 根据inode号，获取目录项的名字
    ///
    /// @param ino inode号
    ///
    /// @return 成功：Ok()
    ///         失败：Err(错误码)
    fn get_entry_name_and_metadata(&self, ino: InodeId) -> Result<(String, Metadata), i32> {
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

impl dyn IndexNode {
    /// @brief 将当前Inode转换为一个具体的结构体（类型由T指定）
    /// 如果类型正确，则返回Some,否则返回None
    pub fn downcast_ref<T: IndexNode>(&self) -> Option<&T> {
        return self.as_any_ref().downcast_ref::<T>();
    }

    pub fn list(&self) -> Result<Vec<String>, i32> {
        let info = self.metadata()?;
        if info.file_type != FileType::Dir {
            return Err(-(ENOTDIR as i32));
        }

        // 通过遍历所有inode号的方式，获得当前inode下面的所有子目录项
        // TODO: 使用更高效的方式获取
        let result: Vec<String> = (0..)
            .map(|ino| self.get_entry_name(ino))
            .take_while(|result| result.is_ok())
            .filter_map(|result| result.ok())
            .collect();
        return Ok(result);
    }

    /// @brief 查找文件（不考虑符号链接）
    /// 
    /// @param path 文件路径
    /// 
    /// @return Ok(Arc<dyn IndexNode>) 要寻找的目录项的inode
    /// @return Err(i32) 错误码
    pub fn lookup(&self, path: &str) -> Result<Arc<dyn IndexNode>, i32> {
        return self.lookup_follow_symlink(path, 0);
    }

    /// @brief 查找文件（考虑符号链接）
    /// 
    /// @param path 文件路径
    /// @param max_follow_times 最大经过的符号链接的大小
    /// 
    /// @return Ok(Arc<dyn IndexNode>) 要寻找的目录项的inode
    /// @return Err(i32) 错误码
    pub fn lookup_follow_symlink(
        &self,
        path: &str,
        max_follow_times: usize,
    ) -> Result<Arc<dyn IndexNode>, i32> {
        if self.metadata()?.file_type != FileType::Dir {
            return Err(-(ENOTDIR as i32));
        }

        // 处理绝对路径
        // result: 上一个被找到的inode
        // rest_path: 还没有查找的路径
        let (mut result, mut rest_path) = if let Some(rest) = path.strip_prefix('/') {
            (self.fs().get_root_inode(), String::from(rest))
        } else {
            // 是相对路径
            (self.find(".")?, String::from(path))
        };

        // 逐级查找文件
        while !rest_path.is_empty() {
            // 当前这一级不是文件夹
            if result.metadata()?.file_type != FileType::Dir {
                return Err(-(ENOTDIR as i32));
            }

            let name;

            // 寻找“/”
            match rest_path.find('/') {
                Some(pos) => {
                    // 找到了，设置下一个要查找的名字
                    name = String::from(&rest_path[0..pos]);
                    // 剩余的路径字符串
                    rest_path = String::from(&rest_path[pos + 1..]);
                }
                None => {
                    name = rest_path;
                    rest_path = String::new();
                }
            }

            // 遇到连续多个"/"的情况
            if name.is_empty() {
                continue;
            }

            let inode = result.find(&name)?;

            // 处理符号链接的问题
            if inode.metadata()?.file_type == FileType::SymLink && max_follow_times > 0 {
                let mut content = [0u8; 256];
                // 读取符号链接
                let len = inode.read_at(0, 256, &mut content)?;

                // 将读到的数据转换为utf8字符串（先转为str，再转为String）
                let link_path = String::from(
                    ::core::str::from_utf8(&content[..len]).map_err(|_| -(ENOTDIR as i32))?,
                );

                let new_path = link_path + "/" + &rest_path;
                // 继续查找符号链接
                return result.lookup_follow_symlink(&new_path, max_follow_times - 1);
            }else {
                result = inode;
            }
        }

        return Ok(result);
    }
}

/// IndexNode的元数据
///
/// 对应Posix2008中的sys/stat.h中的定义 https://pubs.opengroup.org/onlinepubs/9699919799/basedefs/sys_stat.h.html
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Metadata {
    /// 当前inode所在的文件系统的设备号
    pub dev_id: usize,
    /// inode号
    pub inode_id: InodeId,

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
pub trait FileSystem: Any + Sync + Send + Debug {
    /// @brief 获取当前文件系统的root inode的指针
    fn get_root_inode(&self) -> Arc<dyn IndexNode>;

    /// @brief 获取当前文件系统的信息
    fn info(&self) -> FsInfo;

    /// @brief 本函数用于实现动态转换。
    /// 具体的文件系统在实现本函数时，最简单的方式就是：直接返回self
    fn as_any_ref(&self) -> &dyn Any;
}

#[derive(Debug)]
pub struct FsInfo {
    /// 文件系统所在的块设备的id
    pub blk_dev_id: usize,
    /// 文件名的最大长度
    pub max_name_len: usize,
}

