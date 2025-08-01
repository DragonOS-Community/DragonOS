/// 导出devfs的模块
pub mod null_dev;
pub mod zero_dev;

use super::vfs::{
    file::FileMode,
    syscall::ModeType,
    utils::DName,
    vcore::{generate_inode_id, ROOT_INODE},
    FilePrivateData, FileSystem, FileType, FsInfo, IndexNode, Magic, Metadata, SuperBlock,
};
use crate::{
    driver::base::{block::gendisk::GenDisk, device::device_number::DeviceNumber},
    libs::{
        once::Once,
        spinlock::{SpinLock, SpinLockGuard},
    },
    time::PosixTimeSpec,
};
use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use log::{error, info};
use system_error::SystemError;

const DEVFS_BLOCK_SIZE: u64 = 512;
const DEVFS_MAX_NAMELEN: usize = 255;
/// @brief dev文件系统
#[derive(Debug)]
pub struct DevFS {
    // 文件系统根节点
    root_inode: Arc<LockedDevFSInode>,
    super_block: SuperBlock,
}

impl FileSystem for DevFS {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn root_inode(&self) -> Arc<dyn super::vfs::IndexNode> {
        return self.root_inode.clone();
    }

    fn info(&self) -> super::vfs::FsInfo {
        return FsInfo {
            blk_dev_id: 0,
            max_name_len: DEVFS_MAX_NAMELEN,
        };
    }

    fn name(&self) -> &str {
        "devfs"
    }

    fn super_block(&self) -> SuperBlock {
        self.super_block.clone()
    }
}

impl DevFS {
    pub fn new() -> Arc<Self> {
        let super_block = SuperBlock::new(
            Magic::DEVFS_MAGIC,
            DEVFS_BLOCK_SIZE,
            DEVFS_MAX_NAMELEN as u64,
        );
        // 初始化root inode
        let root: Arc<LockedDevFSInode> = Arc::new(LockedDevFSInode(SpinLock::new(
            // /dev 的权限设置为 读+执行，root 可以读写
            // root 的 parent 是空指针
            DevFSInode::new(FileType::Dir, ModeType::from_bits_truncate(0o755), 0),
        )));

        let devfs: Arc<DevFS> = Arc::new(DevFS {
            root_inode: root,
            super_block,
        });

        // 对root inode加锁，并继续完成初始化工作
        let mut root_guard: SpinLockGuard<DevFSInode> = devfs.root_inode.0.lock();
        root_guard.parent = Arc::downgrade(&devfs.root_inode);
        root_guard.self_ref = Arc::downgrade(&devfs.root_inode);
        root_guard.fs = Arc::downgrade(&devfs);
        // 释放锁
        drop(root_guard);

        // 创建文件夹
        let root: &Arc<LockedDevFSInode> = &devfs.root_inode;
        root.add_dir("char")
            .expect("DevFS: Failed to create /dev/char");

        root.add_dir("block")
            .expect("DevFS: Failed to create /dev/block");
        devfs.register_bultinin_device();

        // debug!("ls /dev: {:?}", root.list());
        return devfs;
    }

    /// @brief 注册系统内部自带的设备
    fn register_bultinin_device(&self) {
        use null_dev::LockedNullInode;
        use zero_dev::LockedZeroInode;
        let dev_root: Arc<LockedDevFSInode> = self.root_inode.clone();
        dev_root
            .add_dev("null", LockedNullInode::new())
            .expect("DevFS: Failed to register /dev/null");
        dev_root
            .add_dev("zero", LockedZeroInode::new())
            .expect("DevFS: Failed to register /dev/zero");
    }

    /// @brief 在devfs内注册设备
    ///
    /// @param name 设备名称
    /// @param device 设备节点的结构体
    pub fn register_device<T: DeviceINode>(
        &self,
        name: &str,
        device: Arc<T>,
    ) -> Result<(), SystemError> {
        let dev_root_inode: Arc<LockedDevFSInode> = self.root_inode.clone();
        let metadata = device.metadata()?;
        match metadata.file_type {
            // 字节设备挂载在 /dev/char
            FileType::CharDevice => {
                if dev_root_inode.find("char").is_err() {
                    dev_root_inode.create(
                        "char",
                        FileType::Dir,
                        ModeType::from_bits_truncate(0o755),
                    )?;
                }

                let any_char_inode = dev_root_inode.find("char")?;
                let dev_char_inode: &LockedDevFSInode = any_char_inode
                    .as_any_ref()
                    .downcast_ref::<LockedDevFSInode>()
                    .unwrap();

                // 特殊处理 tty 设备，挂载在 /dev 下
                if name.starts_with("tty") && name.len() >= 3 {
                    dev_root_inode.add_dev(name, device.clone())?;
                } else if name.starts_with("hvc") && name.len() > 3 {
                    // 特殊处理 hvc 设备，挂载在 /dev 下
                    dev_root_inode.add_dev(name, device.clone())?;
                } else if name == "console" {
                    dev_root_inode.add_dev(name, device.clone())?;
                } else if name == "ptmx" {
                    // ptmx设备
                    dev_root_inode.add_dev(name, device.clone())?;
                } else {
                    // 在 /dev/char 下创建设备节点
                    dev_char_inode.add_dev(name, device.clone())?;
                }
                device.set_fs(dev_char_inode.0.lock().fs.clone());
            }
            FileType::BlockDevice => {
                if dev_root_inode.find("block").is_err() {
                    dev_root_inode.create(
                        "block",
                        FileType::Dir,
                        ModeType::from_bits_truncate(0o755),
                    )?;
                }

                let any_block_inode = dev_root_inode.find("block")?;
                let dev_block_inode: &LockedDevFSInode = any_block_inode
                    .as_any_ref()
                    .downcast_ref::<LockedDevFSInode>()
                    .unwrap();

                if name.starts_with("vd") && name.len() > 2 {
                    // 虚拟磁盘设备挂载在 /dev 下
                    dev_root_inode.add_dev(name, device.clone())?;
                    let path = format!("/dev/{}", name);
                    let symlink_name = device
                        .as_any_ref()
                        .downcast_ref::<GenDisk>()
                        .unwrap()
                        .symlink_name();

                    dev_block_inode.add_dev_symlink(&path, &symlink_name)?;
                } else if name.starts_with("nvme") {
                    // NVMe设备挂载在 /dev 下
                    dev_root_inode.add_dev(name, device.clone())?;
                } else {
                    dev_block_inode.add_dev(name, device.clone())?;
                }
                device.set_fs(dev_block_inode.0.lock().fs.clone());
            }
            FileType::KvmDevice => {
                dev_root_inode
                    .add_dev(name, device.clone())
                    .expect("DevFS: Failed to register /dev/kvm");
            }
            FileType::FramebufferDevice => {
                dev_root_inode
                    .add_dev(name, device.clone())
                    .expect("DevFS: Failed to register /dev/fb");
            }
            _ => {
                return Err(SystemError::ENOSYS);
            }
        }

        return Ok(());
    }

    /// @brief 卸载设备
    pub fn unregister_device<T: DeviceINode>(
        &self,
        name: &str,
        device: Arc<T>,
    ) -> Result<(), SystemError> {
        let dev_root_inode: Arc<LockedDevFSInode> = self.root_inode.clone();
        match device.metadata().unwrap().file_type {
            // 字节设备挂载在 /dev/char
            FileType::CharDevice => {
                if dev_root_inode.find("char").is_err() {
                    return Err(SystemError::ENOENT);
                }

                let any_char_inode = dev_root_inode.find("char")?;
                let dev_char_inode = any_char_inode
                    .as_any_ref()
                    .downcast_ref::<LockedDevFSInode>()
                    .unwrap();
                // TODO： 调用设备的卸载接口（当引入卸载接口之后）
                dev_char_inode.remove(name)?;
            }
            FileType::BlockDevice => {
                if dev_root_inode.find("block").is_err() {
                    return Err(SystemError::ENOENT);
                }

                let any_block_inode = dev_root_inode.find("block")?;
                let dev_block_inode = any_block_inode
                    .as_any_ref()
                    .downcast_ref::<LockedDevFSInode>()
                    .unwrap();

                dev_block_inode.remove(name)?;
            }
            _ => {
                return Err(SystemError::ENOSYS);
            }
        }

        return Ok(());
    }
}

/// @brief dev文件i节点(锁)
#[derive(Debug)]
pub struct LockedDevFSInode(SpinLock<DevFSInode>);

/// @brief dev文件i节点(无锁)
#[derive(Debug)]
pub struct DevFSInode {
    /// 指向父Inode的弱引用
    parent: Weak<LockedDevFSInode>,
    /// 指向自身的弱引用
    self_ref: Weak<LockedDevFSInode>,
    /// 子Inode的B树
    children: BTreeMap<DName, Arc<dyn IndexNode>>,
    /// 指向inode所在的文件系统对象的指针
    fs: Weak<DevFS>,
    /// INode 元数据
    metadata: Metadata,
    /// 目录名
    dname: DName,
    /// 当前inode的数据部分(仅供symlink使用)
    data: Vec<u8>,
}

impl DevFSInode {
    pub fn new(dev_type_: FileType, mode: ModeType, data_: usize) -> Self {
        return Self::new_with_parent(Weak::default(), dev_type_, mode, data_);
    }

    pub fn new_with_parent(
        parent: Weak<LockedDevFSInode>,
        dev_type_: FileType,
        mode: ModeType,
        data_: usize,
    ) -> Self {
        return DevFSInode {
            parent,
            self_ref: Weak::default(),
            children: BTreeMap::new(),
            metadata: Metadata {
                dev_id: 1,
                inode_id: generate_inode_id(),
                size: 0,
                blk_size: 0,
                blocks: 0,
                atime: PosixTimeSpec::default(),
                mtime: PosixTimeSpec::default(),
                ctime: PosixTimeSpec::default(),
                btime: PosixTimeSpec::default(),
                file_type: dev_type_, // 文件夹
                mode,
                nlinks: 1,
                uid: 0,
                gid: 0,
                raw_dev: DeviceNumber::from(data_ as u32),
            },
            fs: Weak::default(),
            dname: DName::default(),
            data: Vec::new(),
        };
    }
}

impl LockedDevFSInode {
    pub fn add_dir(&self, name: &str) -> Result<(), SystemError> {
        let guard: SpinLockGuard<DevFSInode> = self.0.lock();

        if guard.children.contains_key(&DName::from(name)) {
            return Err(SystemError::EEXIST);
        }

        match self.do_create_with_data(
            guard,
            name,
            FileType::Dir,
            ModeType::from_bits_truncate(0o755),
            0,
        ) {
            Ok(inode) => inode,
            Err(err) => {
                return Err(err);
            }
        };

        return Ok(());
    }

    pub fn add_dev(&self, name: &str, dev: Arc<dyn IndexNode>) -> Result<(), SystemError> {
        let mut this = self.0.lock();
        let name = DName::from(name);
        if this.children.contains_key(&name) {
            return Err(SystemError::EEXIST);
        }

        this.children.insert(name, dev);
        return Ok(());
    }

    /// # 在devfs中添加一个符号链接
    ///
    /// ## 参数
    /// - `path`: 符号链接指向的路径
    /// - `symlink_name`: 符号链接的名称
    pub fn add_dev_symlink(&self, path: &str, symlink_name: &str) -> Result<(), SystemError> {
        let new_inode = self.create_with_data(
            symlink_name,
            FileType::SymLink,
            ModeType::from_bits_truncate(0o777),
            0,
        )?;

        let buf = path.as_bytes();
        let len = buf.len();
        new_inode
            .downcast_ref::<LockedDevFSInode>()
            .unwrap()
            .write_at(0, len, buf, SpinLock::new(FilePrivateData::Unused).lock())?;
        Ok(())
    }

    pub fn remove(&self, name: &str) -> Result<(), SystemError> {
        let x = self
            .0
            .lock()
            .children
            .remove(&DName::from(name))
            .ok_or(SystemError::ENOENT)?;

        drop(x);
        return Ok(());
    }

    fn do_create_with_data(
        &self,
        mut guard: SpinLockGuard<DevFSInode>,
        name: &str,
        file_type: FileType,
        mode: ModeType,
        dev: usize,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        if guard.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }
        let name = DName::from(name);
        // 如果有重名的，则返回
        if guard.children.contains_key(&name) {
            return Err(SystemError::EEXIST);
        }

        // 创建inode
        let result: Arc<LockedDevFSInode> = Arc::new(LockedDevFSInode(SpinLock::new(DevFSInode {
            parent: guard.self_ref.clone(),
            self_ref: Weak::default(),
            children: BTreeMap::new(),
            metadata: Metadata {
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
                raw_dev: DeviceNumber::from(dev as u32),
            },
            fs: guard.fs.clone(),
            dname: name.clone(),
            data: Vec::new(),
        })));

        // 初始化inode的自引用的weak指针
        result.0.lock().self_ref = Arc::downgrade(&result);

        // 将子inode插入父inode的B树中
        guard.children.insert(name, result.clone());
        return Ok(result);
    }
}

impl IndexNode for LockedDevFSInode {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn open(
        &self,
        _data: SpinLockGuard<FilePrivateData>,
        _mode: &FileMode,
    ) -> Result<(), SystemError> {
        return Ok(());
    }

    fn close(&self, _data: SpinLockGuard<FilePrivateData>) -> Result<(), SystemError> {
        return Ok(());
    }

    fn create_with_data(
        &self,
        name: &str,
        file_type: FileType,
        mode: ModeType,
        data: usize,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        // 获取当前inode
        let guard: SpinLockGuard<DevFSInode> = self.0.lock();
        // 如果当前inode不是文件夹，则返回
        return self.do_create_with_data(guard, name, file_type, mode, data);
    }

    fn find(&self, name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        let inode = self.0.lock();

        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        match name {
            "" | "." => {
                return Ok(inode.self_ref.upgrade().ok_or(SystemError::ENOENT)?);
            }
            ".." => {
                return Ok(inode.parent.upgrade().ok_or(SystemError::ENOENT)?);
            }
            name => {
                // 在子目录项中查找
                return Ok(inode
                    .children
                    .get(&DName::from(name))
                    .ok_or(SystemError::ENOENT)?
                    .clone());
            }
        }
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        return self.0.lock().fs.upgrade().unwrap();
    }

    fn get_entry_name(&self, ino: super::vfs::InodeId) -> Result<String, SystemError> {
        let inode: SpinLockGuard<DevFSInode> = self.0.lock();
        if inode.metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        match ino.into() {
            0 => {
                return Ok(String::from("."));
            }
            1 => {
                return Ok(String::from(".."));
            }
            ino => {
                // 暴力遍历所有的children，判断inode id是否相同
                // TODO: 优化这里，这个地方性能很差！
                let mut key: Vec<String> = inode
                    .children
                    .iter()
                    .filter_map(|(k, v)| {
                        if v.metadata().unwrap().inode_id.into() == ino {
                            Some(k.to_string())
                        } else {
                            None
                        }
                    })
                    .collect();

                match key.len() {
                    0=>{return Err(SystemError::ENOENT);}
                    1=>{return Ok(key.remove(0));}
                    _ => panic!("Devfs get_entry_name: key.len()={key_len}>1, current inode_id={inode_id:?}, to find={to_find:?}", key_len=key.len(), inode_id = inode.metadata.inode_id, to_find=ino)
                }
            }
        }
    }

    fn ioctl(
        &self,
        _cmd: u32,
        _data: usize,
        _private_data: &FilePrivateData,
    ) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn list(&self) -> Result<Vec<String>, SystemError> {
        let info = self.metadata()?;
        if info.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        let mut keys: Vec<String> = Vec::new();
        keys.push(String::from("."));
        keys.push(String::from(".."));
        keys.append(
            &mut self
                .0
                .lock()
                .children
                .keys()
                .map(ToString::to_string)
                .collect(),
        );

        return Ok(keys);
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        return Ok(self.0.lock().metadata.clone());
    }

    fn set_metadata(&self, metadata: &Metadata) -> Result<(), SystemError> {
        let mut inode = self.0.lock();
        inode.metadata.atime = metadata.atime;
        inode.metadata.mtime = metadata.mtime;
        inode.metadata.ctime = metadata.ctime;
        inode.metadata.btime = metadata.btime;
        inode.metadata.mode = metadata.mode;
        inode.metadata.uid = metadata.uid;
        inode.metadata.gid = metadata.gid;

        return Ok(());
    }

    /// 读设备 - 应该调用设备的函数读写，而不是通过文件系统读写，仅支持符号链接的读取
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let meta = self.metadata()?;
        match meta.file_type {
            FileType::SymLink => {
                if buf.len() < len {
                    return Err(SystemError::EINVAL);
                }
                // 加锁
                let inode = self.0.lock();

                // 检查当前inode是否为一个文件夹，如果是的话，就返回错误
                if inode.metadata.file_type == FileType::Dir {
                    return Err(SystemError::EISDIR);
                }

                let start = inode.data.len().min(offset);
                let end = inode.data.len().min(offset + len);

                // buffer空间不足
                if buf.len() < (end - start) {
                    return Err(SystemError::ENOBUFS);
                }

                // 拷贝数据
                let src = &inode.data[start..end];
                buf[0..src.len()].copy_from_slice(src);
                return Ok(src.len());
            }
            _ => {
                error!("DevFS: read_at is not supported!");
                Err(SystemError::ENOSYS)
            }
        }
    }

    /// 写设备 - 应该调用设备的函数读写，而不是通过文件系统读写，仅支持符号链接的写入
    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let meta = self.metadata()?;
        match meta.file_type {
            FileType::SymLink => {
                if buf.len() < len {
                    return Err(SystemError::EINVAL);
                }
                let mut inode = self.0.lock();

                if inode.metadata.file_type == FileType::Dir {
                    return Err(SystemError::EISDIR);
                }

                let data: &mut Vec<u8> = &mut inode.data;

                // 如果文件大小比原来的大，那就resize这个数组
                if offset + len > data.len() {
                    data.resize(offset + len, 0);
                }

                let target = &mut data[offset..offset + len];
                target.copy_from_slice(&buf[0..len]);
                return Ok(len);
            }
            _ => {
                error!("DevFS: read_at is not supported!");
                Err(SystemError::ENOSYS)
            }
        }
    }

    fn parent(&self) -> Result<Arc<dyn IndexNode>, SystemError> {
        let me = self.0.lock();
        Ok(me
            .parent
            .upgrade()
            .unwrap_or(me.self_ref.upgrade().unwrap()))
    }

    fn dname(&self) -> Result<DName, SystemError> {
        Ok(self.0.lock().dname.clone())
    }
}

/// @brief 所有的设备INode都需要额外实现这个trait
pub trait DeviceINode: IndexNode {
    fn set_fs(&self, fs: Weak<DevFS>);
    // TODO: 增加 unregister 方法
}

/// @brief 获取devfs实例的强类型不可变引用
macro_rules! devfs_exact_ref {
    () => {{
        let devfs_inode: Result<Arc<dyn IndexNode>, SystemError> = ROOT_INODE().find("dev");
        if let Err(e) = devfs_inode {
            error!("failed to get DevFS ref. errcode = {:?}", e);
            return Err(SystemError::ENOENT);
        }

        let binding = devfs_inode.unwrap();
        let devfs_inode: &LockedDevFSInode = binding
            .as_any_ref()
            .downcast_ref::<LockedDevFSInode>()
            .unwrap();
        let binding = devfs_inode.fs();
        binding
    }
    .as_any_ref()
    .downcast_ref::<DevFS>()
    .unwrap()};
}
/// @brief devfs的设备注册函数
pub fn devfs_register<T: DeviceINode>(name: &str, device: Arc<T>) -> Result<(), SystemError> {
    return devfs_exact_ref!().register_device(name, device);
}

/// @brief devfs的设备卸载函数
#[allow(dead_code)]
pub fn devfs_unregister<T: DeviceINode>(name: &str, device: Arc<T>) -> Result<(), SystemError> {
    return devfs_exact_ref!().unregister_device(name, device);
}

pub fn devfs_init() -> Result<(), SystemError> {
    static INIT: Once = Once::new();
    let mut result = None;
    INIT.call_once(|| {
        info!("Initializing DevFS...");
        // 创建 devfs 实例
        let devfs: Arc<DevFS> = DevFS::new();
        // devfs 挂载
        ROOT_INODE()
            .mkdir("dev", ModeType::from_bits_truncate(0o755))
            .expect("Unabled to find /dev")
            .mount(devfs)
            .expect("Failed to mount at /dev");
        info!("DevFS mounted.");
        result = Some(Ok(()));
    });

    return result.unwrap();
}
