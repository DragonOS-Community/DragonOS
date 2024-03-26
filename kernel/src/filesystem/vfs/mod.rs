pub mod cache;
pub mod core;
pub mod fcntl;
pub mod file;
pub mod mount;
pub mod open;
pub mod syscall;
mod utils;

use ::core::{any::Any, fmt::Debug, sync::atomic::AtomicUsize};
use alloc::{string::String, sync::Arc, vec::Vec};
use path_base::{clean_path::Clean, Path, PathBuf};
use system_error::SystemError;

use crate::{
    driver::base::{
        block::block_device::BlockDevice, char::CharDevice, device::device_number::DeviceNumber,
    },
    filesystem::vfs::mount::MOUNTS_LIST,
    ipc::pipe::LockedPipeInode,
    libs::casting::DowncastArc,
    time::TimeSpec,
};

use self::{
    cache::DefaultCache,
    core::generate_inode_id,
    file::FileMode,
    syscall::ModeType,
};
pub use self::{core::ROOT_INODE, file::FilePrivateData, mount::MountFS};

/// vfs容许的最大的路径名称长度
pub const MAX_PATHLEN: usize = 1024;

// 定义inode号
int_like!(InodeId, AtomicInodeId, usize, AtomicUsize);

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
    /// 帧缓冲设备
    FramebufferDevice,
    /// kvm设备
    KvmDevice,
    /// 管道文件
    Pipe,
    /// 符号链接
    SymLink,
    /// 套接字
    Socket,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum SpecialNodeData {
    /// 管道文件
    Pipe(Arc<LockedPipeInode>),
    /// 字符设备
    CharDevice(Arc<dyn CharDevice>),
    /// 块设备
    BlockDevice(Arc<dyn BlockDevice>),
}

/* these are defined by POSIX and also present in glibc's dirent.h */
/// 完整含义请见 http://www.gnu.org/software/libc/manual/html_node/Directory-Entries.html
#[allow(dead_code)]
pub const DT_UNKNOWN: u16 = 0;
/// 命名管道，或者FIFO
pub const DT_FIFO: u16 = 1;
// 字符设备
pub const DT_CHR: u16 = 2;
// 目录
pub const DT_DIR: u16 = 4;
// 块设备
pub const DT_BLK: u16 = 6;
// 常规文件
pub const DT_REG: u16 = 8;
// 符号链接
pub const DT_LNK: u16 = 10;
// 是一个socket
pub const DT_SOCK: u16 = 12;
// 这个是抄Linux的，还不知道含义
#[allow(dead_code)]
pub const DT_WHT: u16 = 14;
#[allow(dead_code)]
pub const DT_MAX: u16 = 16;

/// vfs容许的最大的符号链接跳转次数
pub const VFS_MAX_FOLLOW_SYMLINK_TIMES: usize = 8;

impl FileType {
    pub fn get_file_type_num(&self) -> u16 {
        return match self {
            FileType::File => DT_REG,
            FileType::Dir => DT_DIR,
            FileType::BlockDevice => DT_BLK,
            FileType::CharDevice => DT_CHR,
            FileType::KvmDevice => DT_CHR,
            FileType::Pipe => DT_FIFO,
            FileType::SymLink => DT_LNK,
            FileType::Socket => DT_SOCK,
            FileType::FramebufferDevice => DT_CHR,
        };
    }
}

bitflags! {
    /// @brief inode的状态（由poll方法返回）
    pub struct PollStatus: u8 {
        const WRITE = 1u8 << 0;
        const READ = 1u8 << 1;
        const ERROR = 1u8 << 2;
    }
}

pub trait IndexNode: Any + Sync + Send + Debug {
    /// @brief 打开文件
    ///
    /// @return 成功：Ok()
    ///         失败：Err(错误码)
    fn open(&self, _data: &mut FilePrivateData, _mode: &FileMode) -> Result<(), SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    /// @brief 关闭文件
    ///
    /// @return 成功：Ok()
    ///         失败：Err(错误码)
    fn close(&self, _data: &mut FilePrivateData) -> Result<(), SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    /// @brief 在inode的指定偏移量开始，读取指定大小的数据
    ///
    /// @param offset 起始位置在Inode中的偏移量
    /// @param len 要读取的字节数
    /// @param buf 缓冲区. 请注意，必须满足@buf.len()>=@len
    /// @param _data 各文件系统系统所需私有信息
    ///
    /// @return 成功：Ok(读取的字节数)
    ///         失败：Err(Posix错误码)
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: &mut FilePrivateData,
    ) -> Result<usize, SystemError>;

    /// @brief 在inode的指定偏移量开始，写入指定大小的数据（从buf的第0byte开始写入）
    ///
    /// @param offset 起始位置在Inode中的偏移量
    /// @param len 要写入的字节数
    /// @param buf 缓冲区. 请注意，必须满足@buf.len()>=@len
    /// @param _data 各文件系统系统所需私有信息
    ///
    /// @return 成功：Ok(写入的字节数)
    ///         失败：Err(Posix错误码)
    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        _data: &mut FilePrivateData,
    ) -> Result<usize, SystemError>;

    /// @brief 获取当前inode的状态。
    ///
    /// @return PollStatus结构体
    fn poll(&self, _private_data: &FilePrivateData) -> Result<usize, SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    /// @brief 获取inode的元数据
    ///
    /// @return 成功：Ok(inode的元数据)
    ///         失败：Err(错误码)
    fn metadata(&self) -> Result<Metadata, SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    /// @brief 设置inode的元数据
    ///
    /// @return 成功：Ok()
    ///         失败：Err(错误码)
    fn set_metadata(&self, _metadata: &Metadata) -> Result<(), SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    /// @brief 重新设置文件的大小
    ///
    /// 如果文件大小增加，则文件内容不变，但是文件的空洞部分会被填充为0
    /// 如果文件大小减小，则文件内容会被截断
    ///
    /// @return 成功：Ok()
    ///         失败：Err(错误码)
    fn resize(&self, _len: usize) -> Result<(), SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
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
        mode: ModeType,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        // 若文件系统没有实现此方法，则默认调用其create_with_data方法。如果仍未实现，则会得到一个Err(-EOPNOTSUPP_OR_ENOTSUP)的返回值
        return self.create_with_data(name, file_type, mode, 0);
    }

    /// @brief 在当前目录下创建一个新的inode，并传入一个简单的data字段，方便进行初始化。
    ///
    /// @param name 目录项的名字
    /// @param file_type 文件类型
    /// @param mode 权限
    /// @param data 用于初始化该inode的数据。（为0则表示忽略此字段）对于不同的文件系统来说，代表的含义可能不同。
    ///
    /// @return 创建成功：返回Ok(新的inode的Arc指针)
    /// @return 创建失败：返回Err(错误码)
    fn create_with_data(
        &self,
        _name: &str,
        _file_type: FileType,
        _mode: ModeType,
        _data: usize,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    /// @brief 在当前目录下，创建一个名为Name的硬链接，指向另一个IndexNode
    ///
    /// @param name 硬链接的名称
    /// @param other 要被指向的IndexNode的Arc指针
    ///
    /// @return 成功：Ok()
    ///         失败：Err(错误码)
    fn link(&self, _name: &str, _other: &Arc<dyn IndexNode>) -> Result<(), SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    /// @brief 在当前目录下，删除一个名为Name的硬链接
    ///
    /// @param name 硬链接的名称
    ///
    /// @return 成功：Ok()
    ///         失败：Err(错误码)
    fn unlink(&self, _name: &str) -> Result<(), SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    /// @brief 删除文件夹
    ///
    /// @param name 文件夹名称
    ///
    /// @return 成功 Ok(())
    /// @return 失败 Err(错误码)
    fn rmdir(&self, _name: &str) -> Result<(), SystemError> {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    /// @brief 将指定名称的子目录项的文件内容，移动到target这个目录下。
    /// 如果_old_name所指向的inode与_target的相同，那么则直接执行重命名的操作。
    ///
    /// @param old_name 旧的名字
    ///
    /// @param target 移动到指定的inode
    ///
    /// @param new_name 新的文件名
    ///
    /// @return 成功: Ok()
    ///         失败: Err(错误码)
    fn move_to(
        &self,
        _old_name: &str,
        _target: &Arc<dyn IndexNode>,
        _new_name: &str,
    ) -> Result<(), SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    /// # 修改文件名
    ///
    ///
    /// ## 参数
    ///
    /// - _old_name: 源文件路径
    /// - _new_name: 目标文件路径
    ///
    /// ## 返回值
    /// - Ok(返回值类型): 返回值的说明
    /// - Err(错误值类型): 错误的说明
    ///
    fn rename(&self, _old_name: &str, _new_name: &str) -> Result<(), SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    /// @brief 寻找一个名为Name的inode
    ///
    /// @param name 要寻找的inode的名称
    ///
    /// @return 成功：Ok()
    ///         失败：Err(错误码)
    fn find(&self, _name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    /// @brief 根据inode号，获取子目录项的名字
    ///
    /// @param ino inode号
    ///
    /// @return 成功：Ok()
    ///         失败：Err(错误码)
    fn get_entry_name(&self, _ino: InodeId) -> Result<String, SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    /// @brief 根据inode号，获取子目录项的名字和元数据
    ///
    /// @param ino inode号
    ///
    /// @return 成功：Ok(String, Metadata)
    ///         失败：Err(错误码)
    fn get_entry_name_and_metadata(&self, ino: InodeId) -> Result<(String, Metadata), SystemError> {
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
    fn ioctl(
        &self,
        _cmd: u32,
        _data: usize,
        _private_data: &FilePrivateData,
    ) -> Result<usize, SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    /// @brief 获取inode所在的文件系统的指针
    fn fs(&self) -> Arc<dyn FileSystem>;

    /// @brief 本函数用于实现动态转换。
    /// 具体的文件系统在实现本函数时，最简单的方式就是：直接返回self
    fn as_any_ref(&self) -> &dyn Any;

    /// @brief 列出当前inode下的所有目录项的名字
    fn list(&self) -> Result<Vec<String>, SystemError>;

    /// @brief 在当前Inode下，挂载一个新的文件系统
    /// 请注意！该函数只能被MountFS实现，其他文件系统不应实现这个函数
    fn mount(&self, _fs: Arc<dyn FileSystem>) -> Result<Arc<MountFS>, SystemError> {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    /// @brief 截断当前inode到指定的长度。如果当前文件长度小于len,则不操作。
    ///
    /// @param len 要被截断到的目标长度
    fn truncate(&self, _len: usize) -> Result<(), SystemError> {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    /// @brief 将当前inode的内容同步到具体设备上
    fn sync(&self) -> Result<(), SystemError> {
        return Ok(());
    }

    /// ## 创建一个特殊文件节点
    /// - _filename: 文件名
    /// - _mode: 权限信息
    fn mknod(
        &self,
        _filename: &str,
        _mode: ModeType,
        _dev_t: DeviceNumber,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    /// ## 返回特殊文件的inode
    fn special_node(&self) -> Option<SpecialNodeData> {
        None
    }

    /// 获取目录名
    fn key(&self) -> Result<String, SystemError> {
        self.parent()?.get_entry_name(self.metadata()?.inode_id)
    }

    /// 获取父目录
    fn parent(&self) -> Result<Arc<dyn IndexNode>, SystemError> {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    /// 当前抽象层挂载点下的绝对目录
    fn _abs_path(&self) -> Result<PathBuf, SystemError> {
        let mut inode;
        let mut path_stack = Vec::new();
        path_stack.push(self.key()?);

        match self.parent() {
            Err(e) => match e {
                SystemError:: ENOENT => {
                    return Ok(PathBuf::from("/"));
                }
                e => { 
                    return Err(e);
                }
            },
            Ok(o) => {
                if self.metadata()?.inode_id == o.metadata()?.inode_id {
                    return Ok(PathBuf::from("/"));
                }
                inode = o;
                path_stack.push(inode.key()?);
            }
        }

        while inode.metadata()?.inode_id != ROOT_INODE().metadata()?.inode_id {
            let tmp = inode.parent();
            match tmp {
                Err(e) => match e {
                    SystemError::ENOENT => {
                        break;
                    }
                    e => {
                        return Err(e);
                    }
                },
                Ok(o) => {
                    if inode.metadata()?.inode_id == o.metadata()?.inode_id {
                        break;
                    }
                    inode = o;
                    path_stack.push(inode.key()?);
                }
            }
        }
        path_stack.reverse();
        let mut path = PathBuf::from("/");
        path.extend(path_stack);
        return Ok(path);
    }
}

impl DowncastArc for dyn IndexNode {
    fn as_any_arc(self: Arc<Self>) -> Arc<dyn Any> {
        self
    }
}

impl dyn IndexNode {
    /// @brief 将当前Inode转换为一个具体的结构体（类型由T指定）
    /// 如果类型正确，则返回Some,否则返回None
    pub fn downcast_ref<T: IndexNode>(&self) -> Option<&T> {
        return self.as_any_ref().downcast_ref::<T>();
    }

    /// @brief 查找文件（不考虑符号链接）
    ///
    /// @param path 文件路径
    ///
    /// @return Ok(Arc<dyn IndexNode>) 要寻找的目录项的inode
    /// @return Err(SystemError) 错误码
    pub fn lookup(&self, path: &Path) -> Result<Arc<dyn IndexNode>, SystemError> {
        self.lookup_follow_symlink(path, 0)
    }

    /// @brief 查找文件（考虑符号链接）
    ///
    /// @param path 文件路径
    /// @param max_follow_times 最大经过的符号链接的大小
    ///
    /// @return Ok(Arc<dyn IndexNode>) 要寻找的目录项的inode
    /// @return Err(SystemError) 错误码
    pub fn lookup_follow_symlink(
        &self,
        path: &Path,
        max_follow_times: usize,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        // kdebug!("Looking for path {:?}", path);
        // 拼接绝对路径
        let abs_path = if path.is_absolute() {
            path.clean()
        } else {
            self._abs_path()?.join(path.clean())
        };

        // 检查根目录缓存
        if let Some((root_path, fs)) = MOUNTS_LIST()
            .lock()
            .iter()
            .filter_map(|(key, value)| {
                let strkey = key.as_ref();
                return if abs_path.starts_with(strkey) {
                    Some((strkey, value))
                } else {
                    None
                };
            })
            .next()
        {
            if let Some(fs) = fs.as_any_ref().downcast_ref::<MountFS>() {
                let root_inode = fs.mountpoint_root_inode().find(".")?;
                // kdebug!("FS: {:?}", fs);
                // kdebug!("Root Node: {:?}", root_inode);

                if let Ok(fscache) = fs.cache() {
                    let path_under = path.strip_prefix(root_path).unwrap();
                    if path_under.iter().next().is_none() {
                        return Ok(root_inode);
                    }

                    // Cache record is found
                    if let Some((inode, found_path)) = root_inode.quick_lookup(fscache, &abs_path, root_path) {
                        // kdebug!("Found path: {:?}", found_path);
                        let rest_path = abs_path.strip_prefix(found_path).unwrap();
                        // kdebug!("Rest path {:?}", rest_path);
                        // no path left
                        if rest_path.iter().next().is_none() {
                            return Ok(inode);
                        }
    
                        // some path left, pop prefix '/'
                        return inode.lookup_walk(rest_path, max_follow_times);
                    }
                    // kdebug!("Start lookup walk");
                    // Cache record not found
                    return root_inode.lookup_walk(path_under, max_follow_times);
                }
    
                // 在对应文件系统根开始lookup
                return fs
                    .root_inode()
                    ._lookup_follow_symlink(path.as_os_str(), max_follow_times);
            }
        }

        // Exception Normal lookup, for non-cache fs
        // kdebug!("Depredecated.");
        return self._lookup_follow_symlink(path.as_os_str(), max_follow_times);
    }

    fn _lookup_follow_symlink(
        &self,
        path: &str,
        max_follow_times: usize,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        if self.metadata()?.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        // 处理绝对路径
        // result: 上一个被找到的inode
        // rest_path: 还没有查找的路径
        let (mut result, mut rest_path) = if let Some(rest) = path.strip_prefix('/') {
            (ROOT_INODE().clone(), String::from(rest))
        } else {
            // 是相对路径
            (self.find(".")?, String::from(path))
        };

        // 逐级查找文件
        while !rest_path.is_empty() {
            // 当前这一级不是文件夹
            if result.metadata()?.file_type != FileType::Dir {
                return Err(SystemError::ENOTDIR);
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
                let len = inode.read_at(0, 256, &mut content, &mut FilePrivateData::Unused)?;

                // 将读到的数据转换为utf8字符串（先转为str，再转为String）
                let link_path = String::from(
                    ::core::str::from_utf8(&content[..len]).map_err(|_| SystemError::ENOTDIR)?,
                );

                let new_path = link_path + "/" + &rest_path;
                // 继续查找符号链接
                return result._lookup_follow_symlink(&new_path, max_follow_times - 1);
            }
            result = inode;
        }

        Ok(result)
    }

    fn lookup_walk(
        &self,
        rest_path: &Path,
        max_follow_times: usize,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        // kdebug!("walking though {:?}", rest_path);
        if self.metadata()?.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }
        let child = rest_path.iter().next().unwrap();
        // kdebug!("Lookup {:?}", child);
        match self.find(child) {
            Ok(child_node) => {
                if child_node.metadata()?.file_type == FileType::SymLink && max_follow_times > 0 {
                    // symlink wrapping problem
                    return ROOT_INODE().lookup_follow_symlink(
                        &child_node.from_symlink()?.join(rest_path),
                        max_follow_times - 1,
                    );
                }

                // 潜在优化可能：重复的文件系统缓存获取
                if let Ok(cache) = self.fs().cache() {
                    // kdebug!("Calling cache put with child {}", child);
                    cache.put(child, child_node.clone());
                }

                if let Ok(rest) = rest_path.strip_prefix(child) {
                    if rest.iter().next().is_some() {
                        // kdebug!("Rest {:?}", rest);
                        return child_node.lookup_walk(rest, max_follow_times);
                    }
                }
                // kdebug!("return node {:?}", child_node);

                return Ok(child_node);
            }
            Err(e) => {
                return Err(e);
            }
        }
    }

    fn from_symlink(&self) -> Result<PathBuf, SystemError> {
        let mut content = [0u8; 256];
        let len = self.read_at(0, 256, &mut content, &mut FilePrivateData::Unused)?;

        return Ok(PathBuf::from(
            ::core::str::from_utf8(&content[..len]).map_err(|_| SystemError::ENOTDIR)?,
        ));
    }

    /// @brief 缓存查询
    ///
    /// @param path 文件路径
    /// @return Ok((Arc<dyn IndexNode>, &str))
    /// 要寻找的目录项路径上最长已缓存的inode 与 该inode的绝对路径
    /// @return Err(SystemError) 错误码
    ///
    /// 缓存可能未命中
    fn quick_lookup<'a>(
        &'a self,
        cache: Arc<DefaultCache>,
        abs_path: &'a Path,
        stop_path: &'a Path,
    ) -> Option<(Arc<dyn IndexNode>, &Path)> {
        self._quick_lookup(cache, abs_path, abs_path, stop_path)
    }

    fn _quick_lookup<'a>(
        &'a self,
        cache: Arc<DefaultCache>,
        abs_path: &'a Path,
        rest_path: &'a Path,
        stop_path: &'a Path,
    ) -> Option<(Arc<dyn IndexNode>, &Path)> {
        // kdebug!("Quick lookup: abs {:?}, rest {:?}", abs_path, rest_path);
        let key = abs_path.file_name();

        let result = cache.get(key.unwrap()).find(|src| {
            // kdebug!("Src: {:?}, {:?}; Lookup: {:?}, {:?}", src.key(), src._abs_path(), key, abs_path);
            src._abs_path().unwrap() == abs_path
        });

        if result.is_some() {
            // kdebug!("Hit cache!!!");
            return Some((result.unwrap(), abs_path));
        }

        if let Some(parent) = rest_path.parent() {
            if parent == stop_path {
                return None;
            }
            return self._quick_lookup(cache, abs_path, parent, stop_path);
        }
        return None;
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
    pub mode: ModeType,

    /// 硬链接的数量
    pub nlinks: usize,

    /// User ID
    pub uid: usize,

    /// Group ID
    pub gid: usize,

    /// 文件指向的设备的id（对于设备文件系统来说）
    pub raw_dev: DeviceNumber,
}

impl Default for Metadata {
    fn default() -> Self {
        return Self {
            dev_id: 0,
            inode_id: InodeId::new(0),
            size: 0,
            blk_size: 0,
            blocks: 0,
            atime: TimeSpec::default(),
            mtime: TimeSpec::default(),
            ctime: TimeSpec::default(),
            file_type: FileType::File,
            mode: ModeType::empty(),
            nlinks: 1,
            uid: 0,
            gid: 0,
            raw_dev: DeviceNumber::default(),
        };
    }
}

#[derive(Debug, Clone)]
pub struct SuperBlock {
    // type of filesystem
    pub magic: Magic,
    // optimal transfer block size
    pub bsize: u64,
    // total data blocks in filesystem
    pub blocks: u64,
    // free block in system
    pub bfree: u64,
    // 可供非特权用户使用的空闲块
    pub bavail: u64,
    // total inodes in filesystem
    pub files: u64,
    // free inodes in filesystem
    pub ffree: u64,
    // filesysytem id
    pub fsid: u64,
    // Max length of filename
    pub namelen: u64,
    // fragment size
    pub frsize: u64,
    // mount flags of filesystem
    pub flags: u64,
}

impl SuperBlock {
    pub fn new(magic: Magic, bsize: u64, namelen: u64) -> Self {
        Self {
            magic,
            bsize,
            blocks: 0,
            bfree: 0,
            bavail: 0,
            files: 0,
            ffree: 0,
            fsid: 0,
            namelen,
            frsize: 0,
            flags: 0,
        }
    }
}
bitflags! {
    pub struct Magic: u64 {
        const DEVFS_MAGIC = 0x1373;
        const FAT_MAGIC =  0xf2f52011;
        const KER_MAGIC = 0x3153464b;
        const PROC_MAGIC = 0x9fa0;
        const RAMFS_MAGIC = 0x858458f6;
        const MOUNT_MAGIC = 61267;
    }
}

#[derive(Debug, Clone)]
pub struct SuperBlock {
    // type of filesystem
    pub magic: Magic,
    // optimal transfer block size
    pub bsize: u64,
    // total data blocks in filesystem
    pub blocks: u64,
    // free block in system
    pub bfree: u64,
    // 可供非特权用户使用的空闲块
    pub bavail: u64,
    // total inodes in filesystem
    pub files: u64,
    // free inodes in filesystem
    pub ffree: u64,
    // filesysytem id
    pub fsid: u64,
    // Max length of filename
    pub namelen: u64,
    // fragment size
    pub frsize: u64,
    // mount flags of filesystem
    pub flags: u64,
}

impl SuperBlock {
    pub fn new(magic: Magic, bsize: u64, namelen: u64) -> Self {
        Self {
            magic,
            bsize,
            blocks: 0,
            bfree: 0,
            bavail: 0,
            files: 0,
            ffree: 0,
            fsid: 0,
            namelen,
            frsize: 0,
            flags: 0,
        }
    }
}
bitflags! {
    pub struct Magic: u64 {
        const DEVFS_MAGIC = 0x1373;
        const FAT_MAGIC =  0xf2f52011;
        const KER_MAGIC = 0x3153464b;
        const PROC_MAGIC = 0x9fa0;
        const RAMFS_MAGIC = 0x858458f6;
        const MOUNT_MAGIC = 61267;
    }
}

/// @brief 所有文件系统都应该实现的trait
pub trait FileSystem: Any + Sync + Send + Debug {
    /// @brief 获取当前（self所指）文件系统的root inode的指针，默认为MountFS
    fn root_inode(&self) -> Arc<dyn IndexNode>;

    /// @brief 获取当前文件系统的信息
    fn info(&self) -> FsInfo;

    /// @brief 本函数用于实现动态转换。
    /// 具体的文件系统在实现本函数时，最简单的方式就是：直接返回self
    fn as_any_ref(&self) -> &dyn Any;

    fn name(&self) -> &str;

    fn super_block(&self) -> SuperBlock;

    fn cache(&self) -> Result<Arc<DefaultCache>, SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
}

impl DowncastArc for dyn FileSystem {
    fn as_any_arc(self: Arc<Self>) -> Arc<dyn Any> {
        self
    }
}

#[derive(Debug)]
pub struct FsInfo {
    /// 文件系统所在的块设备的id
    pub blk_dev_id: usize,
    /// 文件名的最大长度
    pub max_name_len: usize,
}

/// @brief
#[repr(C)]
#[derive(Debug)]
pub struct Dirent {
    d_ino: u64,    // 文件序列号
    d_off: i64,    // dir偏移量
    d_reclen: u16, // 目录下的记录数
    d_type: u8,    // entry的类型
    d_name: u8,    // 文件entry的名字(是一个零长数组)， 本字段仅用于占位
}

impl Metadata {
    pub fn new(file_type: FileType, mode: ModeType) -> Self {
        Metadata {
            dev_id: 0,
            inode_id: generate_inode_id(),
            size: 0,
            blk_size: 0,
            blocks: 0,
            atime: TimeSpec::default(),
            mtime: TimeSpec::default(),
            ctime: TimeSpec::default(),
            file_type,
            mode,
            nlinks: 1,
            uid: 0,
            gid: 0,
            raw_dev: DeviceNumber::default(),
        }
    }
}
pub struct FileSystemMaker {
    function: &'static FileSystemNewFunction,
    name: &'static str,
}

impl FileSystemMaker {
    pub const fn new(
        name: &'static str,
        function: &'static FileSystemNewFunction,
    ) -> FileSystemMaker {
        FileSystemMaker { function, name }
    }

    pub fn call(&self) -> Result<Arc<dyn FileSystem>, SystemError> {
        (self.function)()
    }
}

pub type FileSystemNewFunction = fn() -> Result<Arc<dyn FileSystem>, SystemError>;

#[macro_export]
macro_rules! define_filesystem_maker_slice {
    ($name:ident) => {
        #[::linkme::distributed_slice]
        pub static $name: [FileSystemMaker] = [..];
    };
    () => {
        compile_error!("define_filesystem_maker_slice! requires at least one argument: slice_name");
    };
}

/// 调用指定数组中的所有初始化器
#[macro_export]
macro_rules! producefs {
    ($initializer_slice:ident,$filesystem:ident) => {
        match $initializer_slice.iter().find(|&m| m.name == $filesystem) {
            Some(maker) => maker.call(),
            None => {
                kerror!("mismatch filesystem type : {}", $filesystem);
                Err(SystemError::EINVAL)
            }
        }
    };
}

define_filesystem_maker_slice!(FSMAKER);
