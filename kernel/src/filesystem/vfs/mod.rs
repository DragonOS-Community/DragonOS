pub mod fcntl;
pub mod file;
pub mod iov;
pub mod mount;
pub mod open;
pub mod permission;
pub mod stat;
pub mod syscall;
pub mod utils;
pub mod vcore;

use ::core::{any::Any, fmt::Debug, sync::atomic::AtomicUsize};
use alloc::{string::String, sync::Arc, vec::Vec};
use derive_builder::Builder;
use intertrait::CastFromSync;
use mount::MountFlags;
use system_error::SystemError;

use crate::{
    driver::base::{
        block::block_device::BlockDevice, char::CharDevice, device::device_number::DeviceNumber,
    },
    filesystem::{epoll::EPollItem, vfs::permission::PermissionMask},
    ipc::pipe::LockedPipeInode,
    libs::{
        casting::DowncastArc,
        spinlock::{SpinLock, SpinLockGuard},
    },
    mm::{fault::PageFaultMessage, VmFaultReason},
    net::socket::Socket,
    process::ProcessManager,
    time::PosixTimeSpec,
};

use self::{file::FileMode, syscall::ModeType, utils::DName, vcore::generate_inode_id};
pub use self::{file::FilePrivateData, mount::MountFS};

use super::page_cache::PageCache;

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

impl From<FileType> for ModeType {
    fn from(val: FileType) -> Self {
        match val {
            FileType::File => ModeType::S_IFREG,
            FileType::Dir => ModeType::S_IFDIR,
            FileType::BlockDevice => ModeType::S_IFBLK,
            FileType::CharDevice => ModeType::S_IFCHR,
            FileType::SymLink => ModeType::S_IFLNK,
            FileType::Socket => ModeType::S_IFSOCK,
            FileType::Pipe => ModeType::S_IFIFO,
            FileType::KvmDevice => ModeType::S_IFCHR,
            FileType::FramebufferDevice => ModeType::S_IFCHR,
        }
    }
}

impl From<ModeType> for FileType {
    fn from(mode: ModeType) -> Self {
        // 提取文件类型部分
        match mode & ModeType::S_IFMT {
            t if t == ModeType::S_IFREG => FileType::File,
            t if t == ModeType::S_IFDIR => FileType::Dir,
            t if t == ModeType::S_IFBLK => FileType::BlockDevice,
            t if t == ModeType::S_IFCHR => FileType::CharDevice,
            t if t == ModeType::S_IFLNK => FileType::SymLink,
            t if t == ModeType::S_IFSOCK => FileType::Socket,
            t if t == ModeType::S_IFIFO => FileType::Pipe,
            // 默认情况，通常应该不会发生，因为 S_IFMT 应该覆盖所有情况
            _ => FileType::File,
        }
    }
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

/// The pollable inode trait
pub trait PollableInode: Any + Sync + Send + Debug + CastFromSync {
    /// Return the poll status of the inode
    fn poll(&self, private_data: &FilePrivateData) -> Result<usize, SystemError>;
    /// Add an epoll item to the inode
    fn add_epitem(
        &self,
        epitem: Arc<EPollItem>,
        private_data: &FilePrivateData,
    ) -> Result<(), SystemError>;
    /// Remove epitems associated with the epoll
    fn remove_epitem(
        &self,
        epitm: &Arc<EPollItem>,
        private_data: &FilePrivateData,
    ) -> Result<(), SystemError>;
}

pub trait IndexNode: Any + Sync + Send + Debug + CastFromSync {
    fn mmap(&self, _start: usize, _len: usize, _offset: usize) -> Result<(), SystemError> {
        return Err(SystemError::ENOSYS);
    }

    fn read_sync(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize, SystemError> {
        return Err(SystemError::ENOSYS);
    }

    fn write_sync(&self, _offset: usize, _buf: &[u8]) -> Result<usize, SystemError> {
        return Err(SystemError::ENOSYS);
    }

    /// @brief 打开文件
    ///
    /// @return 成功：Ok()
    ///         失败：Err(错误码)
    fn open(
        &self,
        _data: SpinLockGuard<FilePrivateData>,
        _mode: &FileMode,
    ) -> Result<(), SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::ENOSYS);
    }

    /// @brief 关闭文件
    ///
    /// @return 成功：Ok()
    ///         失败：Err(错误码)
    fn close(&self, _data: SpinLockGuard<FilePrivateData>) -> Result<(), SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::ENOSYS);
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
        _data: SpinLockGuard<FilePrivateData>,
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
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError>;

    /// # 在inode的指定偏移量开始，读取指定大小的数据，忽略PageCache
    ///
    /// ## 参数
    ///
    /// - `offset`: 起始位置在Inode中的偏移量
    /// - `len`: 要读取的字节数
    /// - `buf`: 缓冲区
    /// - `data`: 各文件系统系统所需私有信息
    ///
    /// ## 返回值
    ///
    /// - `Ok(usize)``: Ok(读取的字节数)
    /// - `Err(SystemError)``: Err(Posix错误码)
    fn read_direct(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &mut [u8],
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        return Err(SystemError::ENOSYS);
    }

    /// # 在inode的指定偏移量开始，写入指定大小的数据，忽略PageCache
    ///
    /// ## 参数
    ///
    /// - `offset`: 起始位置在Inode中的偏移量
    /// - `len`: 要读取的字节数
    /// - `buf`: 缓冲区
    /// - `data`: 各文件系统系统所需私有信息
    ///
    /// ## 返回值
    ///
    /// - `Ok(usize)``: Ok(读取的字节数)
    /// - `Err(SystemError)``: Err(Posix错误码)
    fn write_direct(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        return Err(SystemError::ENOSYS);
    }

    /// @brief 获取inode的元数据
    ///
    /// @return 成功：Ok(inode的元数据)
    ///         失败：Err(错误码)
    fn metadata(&self) -> Result<Metadata, SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::ENOSYS);
    }

    /// @brief 设置inode的元数据
    ///
    /// @return 成功：Ok()
    ///         失败：Err(错误码)
    fn set_metadata(&self, _metadata: &Metadata) -> Result<(), SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::ENOSYS);
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
        return Err(SystemError::ENOSYS);
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
        // 若文件系统没有实现此方法，则默认调用其create_with_data方法。如果仍未实现，则会得到一个Err(-ENOSYS)的返回值
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
        return Err(SystemError::ENOSYS);
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
        return Err(SystemError::ENOSYS);
    }

    /// @brief 在当前目录下，删除一个名为Name的硬链接
    ///
    /// @param name 硬链接的名称
    ///
    /// @return 成功：Ok()
    ///         失败：Err(错误码)
    fn unlink(&self, _name: &str) -> Result<(), SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::ENOSYS);
    }

    /// @brief 删除文件夹
    ///
    /// @param name 文件夹名称
    ///
    /// @return 成功 Ok(())
    /// @return 失败 Err(错误码)
    fn rmdir(&self, _name: &str) -> Result<(), SystemError> {
        return Err(SystemError::ENOSYS);
    }

    /// 将指定的`old_name`子目录项移动到target目录下, 并予以`new_name`。
    ///
    /// # Behavior
    /// 如果old_name所指向的inode与target的相同，那么则直接**执行重命名的操作**。
    fn move_to(
        &self,
        _old_name: &str,
        _target: &Arc<dyn IndexNode>,
        _new_name: &str,
    ) -> Result<(), SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::ENOSYS);
    }

    /// @brief 寻找一个名为Name的inode
    ///
    /// @param name 要寻找的inode的名称
    ///
    /// @return 成功：Ok()
    ///         失败：Err(错误码)
    fn find(&self, _name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::ENOSYS);
    }

    /// @brief 根据inode号，获取子目录项的名字
    ///
    /// @param ino inode号
    ///
    /// @return 成功：Ok()
    ///         失败：Err(错误码)
    fn get_entry_name(&self, _ino: InodeId) -> Result<String, SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::ENOSYS);
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
        return Err(SystemError::ENOSYS);
    }

    /// @brief 获取inode所在的文件系统的指针
    fn fs(&self) -> Arc<dyn FileSystem>;

    /// @brief 本函数用于实现动态转换。
    /// 具体的文件系统在实现本函数时，最简单的方式就是：直接返回self
    fn as_any_ref(&self) -> &dyn Any;

    /// @brief 列出当前inode下的所有目录项的名字
    fn list(&self) -> Result<Vec<String>, SystemError>;

    /// # mount - 挂载文件系统
    ///
    /// 将给定的文件系统挂载到当前的文件系统节点上。
    ///
    /// 该函数是`MountFS`结构体的实例方法，用于将一个新的文件系统挂载到调用它的`MountFS`实例上。
    ///
    /// ## 参数
    ///
    /// - `fs`: `Arc<dyn FileSystem>` - 要挂载的文件系统的共享引用。
    ///
    /// ## 返回值
    ///
    /// - `Ok(Arc<MountFS>)`: 新的挂载文件系统的共享引用。
    /// - `Err(SystemError)`: 挂载过程中出现的错误。
    ///
    /// ## 错误处理
    ///
    /// - 如果文件系统不是目录类型，则返回`SystemError::ENOTDIR`错误。
    /// - 如果当前路径已经是挂载点，则返回`SystemError::EBUSY`错误。
    ///
    /// ## 副作用
    ///
    /// - 该函数会在`MountFS`实例上创建一个新的挂载点。
    /// - 该函数会在全局的挂载列表中记录新的挂载关系。
    fn mount(
        &self,
        _fs: Arc<dyn FileSystem>,
        _mount_flags: MountFlags,
    ) -> Result<Arc<MountFS>, SystemError> {
        return Err(SystemError::ENOSYS);
    }

    /// # mount_from - 从给定的目录挂载已有挂载信息的文件系统
    ///
    /// 这个函数将一个已有挂载信息的文件系统从给定的目录挂载到当前目录。
    ///
    /// ## 参数
    ///
    /// - `from`: Arc<dyn IndexNode> - 要挂载的目录的引用。
    ///
    /// ## 返回值
    ///
    /// - Ok(Arc<MountFS>): 挂载的新文件系统的引用。
    /// - Err(SystemError): 如果发生错误，返回系统错误。
    ///
    /// ## 错误处理
    ///
    /// - 如果给定的目录不是目录类型，返回`SystemError::ENOTDIR`。
    /// - 如果当前目录已经是挂载点的根目录，返回`SystemError::EBUSY`。
    ///
    /// ## 副作用
    ///
    /// - 系统初始化用，其他情况不应调用此函数
    fn mount_from(&self, _des: Arc<dyn IndexNode>) -> Result<Arc<MountFS>, SystemError> {
        return Err(SystemError::ENOSYS);
    }

    /// # umount - 卸载当前Inode下的文件系统
    ///
    /// 该函数是特定于`MountFS`实现的，其他文件系统不应实现此函数。
    ///
    /// ## 参数
    ///
    /// 无
    ///
    /// ## 返回值
    ///
    /// - Ok(Arc<MountFS>): 卸载的文件系统的引用。
    /// - Err(SystemError): 如果发生错误，返回系统错误。
    ///
    /// ## 行为
    ///
    /// - 查找路径
    /// - 定位到父文件系统的挂载点
    /// - 将挂载点与子文件系统的根进行叠加
    /// - 判断是否为子文件系统的根
    /// - 调用父文件系统挂载点的`_umount`方法进行卸载
    fn umount(&self) -> Result<Arc<MountFS>, SystemError> {
        return Err(SystemError::ENOSYS);
    }

    /// Returns the absolute path of the inode.
    ///
    /// This function only works for `MountFS` and should not be implemented by other file systems.
    /// The performance of this function is O(n) for path queries, and it is extremely
    /// inefficient in file systems that do not implement DName caching.
    ///
    /// **WARNING**
    ///
    /// For special inodes(e.g., sockets,pipes, etc.), this function will
    /// return an special name according to the inode type directly.
    ///
    fn absolute_path(&self) -> Result<String, SystemError> {
        return Err(SystemError::ENOSYS);
    }

    /// @brief 截断当前inode到指定的长度。如果当前文件长度小于len,则不操作。
    ///
    /// @param len 要被截断到的目标长度
    fn truncate(&self, _len: usize) -> Result<(), SystemError> {
        return Err(SystemError::ENOSYS);
    }

    /// @brief 将当前inode的内容同步到具体设备上
    fn sync(&self) -> Result<(), SystemError> {
        let page_cache = self.page_cache();
        if let Some(page_cache) = page_cache {
            return page_cache.lock_irqsave().sync();
        }
        Ok(())
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
        return Err(SystemError::ENOSYS);
    }

    /// # mkdir - 新建名称为`name`的目录项
    ///
    /// 当目录下已有名称为`name`的文件夹时，返回该目录项的引用；否则新建`name`文件夹，并返回该引用。
    ///
    /// 该函数会检查`name`目录是否已存在，如果存在但类型不为文件夹，则会返回`EEXIST`错误。
    ///
    /// # 参数
    ///
    /// - `name`: &str - 要新建的目录项的名称。
    /// - `mode`: ModeType - 设置目录项的权限模式。
    ///
    /// # 返回值
    ///
    /// - `Ok(Arc<dyn IndexNode>)`: 成功时返回`name`目录项的共享引用。
    /// - `Err(SystemError)`: 出错时返回错误信息。
    fn mkdir(&self, name: &str, mode: ModeType) -> Result<Arc<dyn IndexNode>, SystemError> {
        match self.find(name) {
            Ok(inode) => {
                if inode.metadata()?.file_type == FileType::Dir {
                    Ok(inode)
                } else {
                    Err(SystemError::EEXIST)
                }
            }
            Err(SystemError::ENOENT) => self.create(name, FileType::Dir, mode),
            Err(err) => Err(err),
        }
    }

    /// ## 返回特殊文件的inode
    fn special_node(&self) -> Option<SpecialNodeData> {
        None
    }

    /// # dname - 返回目录名
    ///
    /// 此函数用于返回一个目录名。
    ///
    /// ## 参数
    ///
    /// 无
    ///
    /// ## 返回值
    /// - Ok(DName): 成功时返回一个目录名。
    /// - Err(SystemError): 如果系统不支持此操作，则返回一个系统错误。
    fn dname(&self) -> Result<DName, SystemError> {
        return Err(SystemError::ENOSYS);
    }

    /// # parent - 返回父目录的引用
    ///
    /// 当该目录是当前文件系统的根目录时，返回自身的引用。
    ///
    /// ## 参数
    ///
    /// 无
    ///
    /// ## 返回值
    ///
    /// - Ok(Arc<dyn IndexNode>): A reference to the parent directory
    /// - Err(SystemError): If there is an error in finding the parent directory
    fn parent(&self) -> Result<Arc<dyn IndexNode>, SystemError> {
        return self.find("..");
    }

    fn page_cache(&self) -> Option<Arc<PageCache>> {
        log::error!(
            "function page_cache() has not yet been implemented for inode:{}",
            crate::libs::name::get_type_name(&self)
        );
        None
    }

    /// Transform the inode to a pollable inode
    ///
    /// If the inode is not pollable, return an error
    fn as_pollable_inode(&self) -> Result<&dyn PollableInode, SystemError> {
        Err(SystemError::ENOSYS)
    }

    /// @brief 按文件名获取扩展属性
    ///
    /// @param name 属性名称
    /// @param buf 用于存储扩展属性值的缓冲区
    ///
    /// @return 成功：Ok(属性值的实际长度)
    ///         失败：Err(错误码)
    fn getxattr(&self, _name: &str, _buf: &mut [u8]) -> Result<usize, SystemError> {
        log::warn!(
            "getxattr not implemented for {}",
            crate::libs::name::get_type_name(&self)
        );
        return Err(SystemError::ENOSYS);
    }

    /// @brief 按文件名设置扩展属性
    ///
    /// @param name 属性名称
    /// @param buf 用于存储扩展属性值的缓冲区
    /// @param value 要设置的扩展属性值
    ///
    /// @return 成功：Ok(0)
    ///         失败：Err(错误码)
    fn setxattr(&self, _name: &str, _value: &[u8]) -> Result<usize, SystemError> {
        log::warn!(
            "setxattr not implemented for {}",
            crate::libs::name::get_type_name(&self)
        );
        return Err(SystemError::ENOSYS);
    }

    /// # 将当前Inode转换为 Socket 引用
    ///
    /// # 返回值
    /// - Some(&dyn Socket): 当前Inode是Socket类型，返回其引用
    /// - None: 当前Inode不是Socket类型
    ///
    /// # 注意
    /// 这个方法已经为dyn Socket实现，
    /// 所以如果可以确定当前`dyn IndexNode`是`dyn Socket`类型，则可以直接调用此方法进行转换
    fn as_socket(&self) -> Option<&dyn Socket> {
        None
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
    pub fn lookup(&self, path: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        return self.lookup_follow_symlink(path, 0);
    }

    pub fn lookup_follow_symlink(
        &self,
        path: &str,
        max_follow_times: usize,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        return self.do_lookup_follow_symlink(path, max_follow_times, true);
    }

    pub fn lookup_follow_symlink2(
        &self,
        path: &str,
        max_follow_times: usize,
        follow_final_symlink: bool,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        return self.do_lookup_follow_symlink(path, max_follow_times, follow_final_symlink);
    }

    /// # 查找文件
    /// 查找指定路径的文件，考虑符号链接的存在，并可选择是否返回最终路径的符号链接文件本身。
    ///
    /// ## 参数
    /// - `path`: 文件路径
    /// - `max_follow_times`: 最大经过的符号链接的数量
    /// - `follow_final_symlink`: 是否跟随最后的符号链接
    ///
    /// ## 返回值
    /// - `Ok(Arc<dyn IndexNode>)`: 要寻找的目录项的inode
    /// - `Err(SystemError)`: 错误码，表示查找过程中遇到的错误
    ///
    /// ## Safety
    /// 此函数在处理符号链接时可能会遇到循环引用的情况，`max_follow_times` 参数用于限制符号链接的跟随次数以避免无限循环。
    #[inline(never)]
    pub fn do_lookup_follow_symlink(
        &self,
        path: &str,
        max_follow_times: usize,
        follow_final_symlink: bool,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        if self.metadata()?.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        let root_inode = ProcessManager::current_mntns().root_inode();

        // 获取当前进程的凭证（用于路径遍历的权限检查）
        let cred = ProcessManager::current_pcb().cred();

        // 处理绝对路径
        // result: 上一个被找到的inode
        // rest_path: 还没有查找的路径
        let (mut result, mut rest_path) = if let Some(rest) = path.strip_prefix('/') {
            (root_inode.clone(), String::from(rest))
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

            // 检查当前目录的执行权限（搜索权限）
            // 这确保了进程有权限遍历到此目录
            let metadata = result.metadata()?;
            cred.inode_permission(&metadata, PermissionMask::MAY_EXEC.bits())?;

            let name;
            // 寻找“/”
            match rest_path.find('/') {
                Some(pos) => {
                    name = String::from(&rest_path[0..pos]);
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
            let file_type = inode.metadata()?.file_type;
            // 如果已经是路径的最后一个部分，并且不希望跟随最后的符号链接
            if rest_path.is_empty() && !follow_final_symlink && file_type == FileType::SymLink {
                // 返回符号链接本身
                return Ok(inode);
            }

            // 跟随符号链接跳转
            if file_type == FileType::SymLink && max_follow_times > 0 {
                let mut content = [0u8; 256];
                // 读取符号链接
                // TODO:We need to clarify which interfaces require private data and which do not
                let len = inode.read_at(
                    0,
                    256,
                    &mut content,
                    SpinLock::new(FilePrivateData::Unused).lock(),
                )?;

                // 将读到的数据转换为utf8字符串（先转为str，再转为String）
                let link_path = String::from(
                    ::core::str::from_utf8(&content[..len]).map_err(|_| SystemError::EINVAL)?,
                );

                // 拼接路径时，如果rest_path为空，则不添加斜杠
                let new_path = if rest_path.is_empty() {
                    link_path
                } else {
                    link_path + "/" + &rest_path
                };

                // 继续查找符号链接
                return result.lookup_follow_symlink2(
                    &new_path,
                    max_follow_times - 1,
                    follow_final_symlink,
                );
            } else {
                result = inode;
            }
        }

        return Ok(result);
    }
}

/// IndexNode的元数据
///
/// 对应Posix2008中的sys/stat.h中的定义 https://pubs.opengroup.org/onlinepubs/9699919799/basedefs/sys_stat.h.html
#[derive(Debug, PartialEq, Eq, Clone, Builder)]
#[builder(no_std, setter(into))]
pub struct Metadata {
    /// 当前inode所在的文件系统的设备号
    /// todo:更改为DeviceNumber结构体
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
    pub atime: PosixTimeSpec,

    /// inode的文件数据最后一次修改的时间
    pub mtime: PosixTimeSpec,

    /// inode的元数据、权限或文件内容最后一次发生改变的时间
    pub ctime: PosixTimeSpec,

    /// inode的创建时间
    pub btime: PosixTimeSpec,

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
            atime: PosixTimeSpec::default(),
            mtime: PosixTimeSpec::default(),
            ctime: PosixTimeSpec::default(),
            btime: PosixTimeSpec::default(),
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
        const EXT4_MAGIC = 0xef53;
        const KER_MAGIC = 0x3153464b;
        const PROC_MAGIC = 0x9fa0;
        const RAMFS_MAGIC = 0x858458f6;
        const MOUNT_MAGIC = 61267;
    }
}

/// @brief 所有文件系统都应该实现的trait
pub trait FileSystem: Any + Sync + Send + Debug {
    /// @brief 获取当前文件系统的root inode的指针
    fn root_inode(&self) -> Arc<dyn IndexNode>;

    /// @brief 获取当前文件系统的信息
    fn info(&self) -> FsInfo;

    /// @brief 本函数用于实现动态转换。
    /// 具体的文件系统在实现本函数时，最简单的方式就是：直接返回self
    fn as_any_ref(&self) -> &dyn Any;

    fn name(&self) -> &str;

    fn super_block(&self) -> SuperBlock;

    unsafe fn fault(&self, _pfm: &mut PageFaultMessage) -> VmFaultReason {
        panic!(
            "fault() has not yet been implemented for filesystem: {}",
            crate::libs::name::get_type_name(&self)
        )
    }

    unsafe fn map_pages(
        &self,
        _pfm: &mut PageFaultMessage,
        _start_pgoff: usize,
        _end_pgoff: usize,
    ) -> VmFaultReason {
        panic!(
            "map_pages() has not yet been implemented for filesystem: {}",
            crate::libs::name::get_type_name(&self)
        )
    }
}

impl DowncastArc for dyn FileSystem {
    fn as_any_arc(self: Arc<Self>) -> Arc<dyn Any> {
        self
    }
}

/// # 可以被挂载的文件系统应该实现的trait
pub trait MountableFileSystem: FileSystem {
    fn make_mount_data(
        _raw_data: Option<&str>,
        _source: &str,
    ) -> Result<Option<Arc<dyn FileSystemMakerData + 'static>>, SystemError> {
        log::error!("This filesystem does not support make_mount_data");
        Err(SystemError::ENOSYS)
    }

    fn make_fs(
        _data: Option<&dyn FileSystemMakerData>,
    ) -> Result<Arc<dyn FileSystem + 'static>, SystemError> {
        log::error!("This filesystem does not support make_fs");
        Err(SystemError::ENOSYS)
    }
}

/// # 注册一个可以被挂载文件系统
/// 此宏用于注册一个可以被挂载的文件系统。
/// 它会将文件系统的创建函数和挂载数据创建函数注册到全局的`FSMAKER`数组中。
///
/// ## 参数
/// - `$fs`: 文件系统对应的结构体
/// - `$maker_name`: 文件系统的注册名
/// - `$fs_name`: 文件系统的名称（字符串字面量）
#[macro_export]
macro_rules! register_mountable_fs {
    ($fs:ident, $maker_name:ident, $fs_name:literal) => {
        impl $fs {
            fn make_fs_bridge(
                data: Option<&dyn FileSystemMakerData>,
            ) -> Result<Arc<dyn FileSystem>, SystemError> {
                <$fs as MountableFileSystem>::make_fs(data)
            }

            fn make_mount_data_bridge(
                raw_data: Option<&str>,
                source: &str,
            ) -> Result<Option<Arc<dyn FileSystemMakerData + 'static>>, SystemError> {
                <$fs as MountableFileSystem>::make_mount_data(raw_data, source)
            }
        }

        #[distributed_slice(FSMAKER)]
        static $maker_name: FileSystemMaker = FileSystemMaker::new(
            $fs_name,
            &($fs::make_fs_bridge
                as fn(
                    Option<&dyn FileSystemMakerData>,
                ) -> Result<Arc<dyn FileSystem + 'static>, SystemError>),
            &($fs::make_mount_data_bridge
                as fn(
                    Option<&str>,
                    &str,
                )
                    -> Result<Option<Arc<dyn FileSystemMakerData + 'static>>, SystemError>),
        );
    };
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
            atime: PosixTimeSpec::default(),
            mtime: PosixTimeSpec::default(),
            ctime: PosixTimeSpec::default(),
            btime: PosixTimeSpec::default(),
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
    /// 文件系统的创建函数
    maker: &'static FSMakerFunction,
    /// 文件系统的名称
    name: &'static str,
    /// 用于创建挂载数据的函数
    builder: &'static MountDataBuilder,
}

impl FileSystemMaker {
    pub const fn new(
        name: &'static str,
        maker: &'static FSMakerFunction,
        builder: &'static MountDataBuilder,
    ) -> FileSystemMaker {
        FileSystemMaker {
            maker,
            name,
            builder,
        }
    }

    pub fn build(
        &self,
        data: Option<&dyn FileSystemMakerData>,
    ) -> Result<Arc<dyn FileSystem>, SystemError> {
        (self.maker)(data)
    }
}

pub trait FileSystemMakerData: Send + Sync {
    fn as_any(&self) -> &dyn Any;
}

pub type FSMakerFunction =
    fn(data: Option<&dyn FileSystemMakerData>) -> Result<Arc<dyn FileSystem>, SystemError>;
pub type MountDataBuilder =
    fn(
        raw_data: Option<&str>,
        source: &str,
    ) -> Result<Option<Arc<dyn FileSystemMakerData + 'static>>, SystemError>;

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

/// # 通过文件系统的名称和数据创建一个文件系统实例
///
/// ## 参数
/// - `filesystem`: 文件系统的名称
/// - `data`: 可选的挂载数据
/// - `source`: 挂载源
///
/// ## 返回值
/// - `Ok(Arc<dyn FileSystem>)`: 成功时返回文件系统的共享引用
/// - `Err(SystemError)`: 如果找不到对应的文件系统或创建失败，则返回错误
///
/// 这个是之前的`produce_fs!`的函数版本，改成了函数之后ext4的挂载会慢一点，仅作记录
pub fn produce_fs(
    filesystem: &str,
    data: Option<&str>,
    source: &str,
) -> Result<Arc<dyn FileSystem>, SystemError> {
    match FSMAKER.iter().find(|&m| m.name == filesystem) {
        Some(maker) => {
            let mount_data = (maker.builder)(data, source)?;
            let mount_data_ref = mount_data.as_ref().map(|arc| arc.as_ref());
            maker.build(mount_data_ref)
        }
        None => {
            log::error!("mismatch filesystem type : {}", filesystem);
            Err(SystemError::EINVAL)
        }
    }
}

define_filesystem_maker_slice!(FSMAKER);

/// # 批量填充Dirent时的上下文Add commentMore actions
/// linux语义是通过getdents_callback *类型来实现类似链表的迭代填充，这里考虑通过填充传入的缓冲区来实现
pub struct FilldirContext<'a> {
    buf: &'a mut [u8],
    current_pos: usize,
    remain_size: usize,
    error: Option<SystemError>,
}

impl<'a> FilldirContext<'a> {
    pub fn new(user_buf: &'a mut [u8]) -> Self {
        Self {
            remain_size: user_buf.len(),
            buf: user_buf,
            current_pos: 0,
            error: None,
        }
    }

    /// # 填充单个dirent结构体
    ///
    /// ## 参数
    /// - name 目录项名称
    /// - offset 当前目录项偏移量
    /// - ino 目录项的inode的inode_id
    /// - d_type 目录项的inode的file_type_num
    fn fill_dir(
        &mut self,
        name: &str,
        offset: usize,
        ino: u64,
        d_type: u8,
    ) -> Result<(), SystemError> {
        let name_len = name.len();
        let dirent_size = ::core::mem::size_of::<Dirent>() - ::core::mem::size_of::<u8>();
        let reclen = name_len + dirent_size + 1;

        // 将reclen向上对齐usize大小
        let align_up = |len: usize, align: usize| -> usize { (len + align - 1) & !(align - 1) };
        let align_up_reclen = align_up(reclen, ::core::mem::size_of::<usize>());

        // 当前缓冲区空间已不足，返回EINVAL
        if align_up_reclen > self.remain_size {
            self.error = Some(SystemError::EINVAL);
            return Err(SystemError::EINVAL);
        }

        // 获取剩余缓冲区
        let current_dirent_slice = &mut self.buf[self.current_pos..];
        let dirent = unsafe { (current_dirent_slice.as_mut_ptr() as *mut Dirent).as_mut() }
            .ok_or(SystemError::EFAULT)?;

        // 填充dirent
        dirent.d_ino = ino;
        dirent.d_type = d_type;
        dirent.d_reclen = align_up_reclen as u16;
        dirent.d_off = offset as i64;
        unsafe {
            let ptr = &mut dirent.d_name as *mut u8;
            let buf: &mut [u8] =
                ::core::slice::from_raw_parts_mut::<'static, u8>(ptr, name_len + 1);
            buf[0..name_len].copy_from_slice(name.as_bytes());
            buf[name_len] = 0;
        }

        self.current_pos += align_up_reclen;
        self.remain_size -= align_up_reclen;

        return Ok(());
    }
}
