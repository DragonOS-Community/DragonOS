use core::{
    hint::spin_loop,
    ptr::null_mut,
    sync::atomic::{AtomicUsize, Ordering},
};

use alloc::{boxed::Box, format, string::ToString, sync::Arc};

use crate::{
    driver::disk::ahci::{self},
    filesystem::{
        devfs::DevFS,
        fat::fs::FATFileSystem,
        procfs::ProcFS,
        ramfs::RamFS,
        sysfs::SysFS,
        vfs::{mount::MountFS, FileSystem, FileType},
    },
    include::bindings::bindings::PAGE_4K_SIZE,
    kerror, kinfo,
    syscall::SystemError,
};

use super::{file::FileMode, utils::rsplit_path, IndexNode, InodeId};

/// @brief 原子地生成新的Inode号。
/// 请注意，所有的inode号都需要通过该函数来生成.全局的inode号，除了以下两个特殊的以外，都是唯一的
/// 特殊的两个inode号：
/// [0]: 对应'.'目录项
/// [1]: 对应'..'目录项
pub fn generate_inode_id() -> InodeId {
    static INO: AtomicUsize = AtomicUsize::new(1);
    return INO.fetch_add(1, Ordering::SeqCst);
}

static mut __ROOT_INODE: *mut Arc<dyn IndexNode> = null_mut();

/// @brief 获取全局的根节点
#[inline(always)]
#[allow(non_snake_case)]
pub fn ROOT_INODE() -> Arc<dyn IndexNode> {
    unsafe {
        return __ROOT_INODE.as_ref().unwrap().clone();
    }
}

#[no_mangle]
pub extern "C" fn vfs_init() -> i32 {
    // 使用Ramfs作为默认的根文件系统
    let ramfs = RamFS::new();
    let mount_fs = MountFS::new(ramfs, None);
    let root_inode = Box::leak(Box::new(mount_fs.root_inode()));

    unsafe {
        __ROOT_INODE = root_inode;
    }

    // 创建文件夹
    root_inode
        .create("proc", FileType::Dir, 0o777)
        .expect("Failed to create /proc");
    root_inode
        .create("dev", FileType::Dir, 0o777)
        .expect("Failed to create /dev");
    root_inode
        .create("sys", FileType::Dir, 0o777)
        .expect("Failed to create /sys");

    // // 创建procfs实例
    let procfs: Arc<ProcFS> = ProcFS::new();

    // procfs挂载
    let _t = root_inode
        .find("proc")
        .expect("Cannot find /proc")
        .mount(procfs)
        .expect("Failed to mount procfs.");
    kinfo!("ProcFS mounted.");

    // 创建 devfs 实例
    let devfs: Arc<DevFS> = DevFS::new();
    // devfs 挂载
    let _t = root_inode
        .find("dev")
        .expect("Cannot find /dev")
        .mount(devfs)
        .expect("Failed to mount devfs");
    kinfo!("DevFS mounted.");

    // 创建 sysfs 实例
    let sysfs: Arc<SysFS> = SysFS::new();
    // sysfs 挂载
    let _t = root_inode
        .find("sys")
        .expect("Cannot find /sys")
        .mount(sysfs)
        .expect("Failed to mount sysfs");
    kinfo!("SysFS mounted.");

    let root_inode = ROOT_INODE().list().expect("VFS init failed");
    if root_inode.len() > 0 {
        kinfo!("Successfully initialized VFS!");
    }
    return 0;
}

/// @brief 真正执行伪文件系统迁移的过程
///
/// @param mountpoint_name 在根目录下的挂载点的名称
/// @param inode 原本的挂载点的inode
fn do_migrate(
    new_root_inode: Arc<dyn IndexNode>,
    mountpoint_name: &str,
    fs: &MountFS,
) -> Result<(), SystemError> {
    let r = new_root_inode.find(mountpoint_name);
    let mountpoint = if r.is_err() {
        new_root_inode
            .create(mountpoint_name, FileType::Dir, 0o777)
            .expect(format!("Failed to create '/{mountpoint_name}'").as_str())
    } else {
        r.unwrap()
    };
    // 迁移挂载点
    mountpoint
        .mount(fs.inner_filesystem())
        .expect(format!("Failed to migrate {mountpoint_name}").as_str());
    return Ok(());
}

/// @brief 迁移伪文件系统的inode
/// 请注意，为了避免删掉了伪文件系统内的信息，因此没有在原root inode那里调用unlink.
fn migrate_virtual_filesystem(new_fs: Arc<dyn FileSystem>) -> Result<(), SystemError> {
    kinfo!("VFS: Migrating filesystems...");

    // ==== 在这里获取要被迁移的文件系统的inode ===
    let binding = ROOT_INODE().find("proc").expect("ProcFS not mounted!").fs();
    let proc: &MountFS = binding.as_any_ref().downcast_ref::<MountFS>().unwrap();
    let binding = ROOT_INODE().find("dev").expect("DevFS not mounted!").fs();
    let dev: &MountFS = binding.as_any_ref().downcast_ref::<MountFS>().unwrap();
    let binding = ROOT_INODE().find("sys").expect("SysFs not mounted!").fs();
    let sys: &MountFS = binding.as_any_ref().downcast_ref::<MountFS>().unwrap();

    let new_fs = MountFS::new(new_fs, None);
    // 获取新的根文件系统的根节点的引用
    let new_root_inode = Box::leak(Box::new(new_fs.root_inode()));

    // 把上述文件系统,迁移到新的文件系统下
    do_migrate(new_root_inode.clone(), "proc", proc)?;
    do_migrate(new_root_inode.clone(), "dev", dev)?;
    do_migrate(new_root_inode.clone(), "sys", sys)?;

    unsafe {
        // drop旧的Root inode
        let old_root_inode: Box<Arc<dyn IndexNode>> = Box::from_raw(__ROOT_INODE);
        __ROOT_INODE = null_mut();
        drop(old_root_inode);

        // 设置全局的新的ROOT Inode
        __ROOT_INODE = new_root_inode;
    }

    kinfo!("VFS: Migrate filesystems done!");

    return Ok(());
}

#[no_mangle]
pub extern "C" fn mount_root_fs() -> i32 {
    kinfo!("Try to mount FAT32 as root fs...");
    let partiton: Arc<crate::filesystem::vfs::io::disk_info::Partition> =
        ahci::get_disks_by_name("ahci_disk_0".to_string())
            .unwrap()
            .0
            .lock()
            .partitions[0]
            .clone();

    let fatfs: Result<Arc<FATFileSystem>, SystemError> = FATFileSystem::new(partiton);
    if fatfs.is_err() {
        kerror!(
            "Failed to initialize fatfs, code={:?}",
            fatfs.as_ref().err()
        );
        loop {
            spin_loop();
        }
    }
    let fatfs: Arc<FATFileSystem> = fatfs.unwrap();
    let r = migrate_virtual_filesystem(fatfs);
    if r.is_err() {
        kerror!("Failed to migrate virtual filesystem to FAT32!");
        loop {
            spin_loop();
        }
    }
    kinfo!("Successfully migrate rootfs to FAT32!");

    return 0;
}

/// @brief 创建文件/文件夹
pub fn do_mkdir(path: &str, _mode: FileMode) -> Result<u64, SystemError> {
    // 文件名过长
    if path.len() > PAGE_4K_SIZE as usize {
        return Err(SystemError::ENAMETOOLONG);
    }

    let inode: Result<Arc<dyn IndexNode>, SystemError> = ROOT_INODE().lookup(path);

    if inode.is_err() {
        let errno = inode.unwrap_err();
        // 文件不存在，且需要创建
        if errno == SystemError::ENOENT {
            let (filename, parent_path) = rsplit_path(path);
            // 查找父目录
            let parent_inode: Arc<dyn IndexNode> =
                ROOT_INODE().lookup(parent_path.unwrap_or("/"))?;
            // 创建文件夹
            let _create_inode: Arc<dyn IndexNode> =
                parent_inode.create(filename, FileType::Dir, 0o777)?;
        } else {
            // 不需要创建文件，因此返回错误码
            return Err(errno);
        }
    }

    return Ok(0);
}

/// @brief 删除文件夹
pub fn do_remove_dir(path: &str) -> Result<u64, SystemError> {
    // 文件名过长
    if path.len() > PAGE_4K_SIZE as usize {
        return Err(SystemError::ENAMETOOLONG);
    }

    let inode: Result<Arc<dyn IndexNode>, SystemError> = ROOT_INODE().lookup(path);

    if inode.is_err() {
        let errno = inode.unwrap_err();
        // 文件不存在
        if errno == SystemError::ENOENT {
            return Err(SystemError::ENOENT);
        }
    }

    let (filename, parent_path) = rsplit_path(path);
    // 查找父目录
    let parent_inode: Arc<dyn IndexNode> = ROOT_INODE().lookup(parent_path.unwrap_or("/"))?;

    if parent_inode.metadata()?.file_type != FileType::Dir {
        return Err(SystemError::ENOTDIR);
    }

    let target_inode: Arc<dyn IndexNode> = parent_inode.find(filename)?;
    if target_inode.metadata()?.file_type != FileType::Dir {
        return Err(SystemError::ENOTDIR);
    }

    // 删除文件夹
    parent_inode.rmdir(filename)?;

    return Ok(0);
}

/// @brief 删除文件
pub fn do_unlink_at(path: &str, _mode: FileMode) -> Result<u64, SystemError> {
    // 文件名过长
    if path.len() > PAGE_4K_SIZE as usize {
        return Err(SystemError::ENAMETOOLONG);
    }

    let inode: Result<Arc<dyn IndexNode>, SystemError> = ROOT_INODE().lookup(path);

    if inode.is_err() {
        let errno = inode.clone().unwrap_err();
        // 文件不存在，且需要创建
        if errno == SystemError::ENOENT {
            return Err(SystemError::ENOENT);
        }
    }
    // 禁止在目录上unlink
    if inode.unwrap().metadata()?.file_type == FileType::Dir {
        return Err(SystemError::EPERM);
    }

    let (filename, parent_path) = rsplit_path(path);
    // 查找父目录
    let parent_inode: Arc<dyn IndexNode> = ROOT_INODE().lookup(parent_path.unwrap_or("/"))?;

    if parent_inode.metadata()?.file_type != FileType::Dir {
        return Err(SystemError::ENOTDIR);
    }

    // 删除文件
    parent_inode.unlink(filename)?;

    return Ok(0);
}
