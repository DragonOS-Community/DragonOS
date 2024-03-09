use core::{hint::spin_loop, sync::atomic::Ordering};

use alloc::{format, string::ToString, sync::Arc};
use system_error::SystemError;

use crate::{
    driver::{
        base::block::disk_info::Partition,
        disk::ahci::{self},
    },
    filesystem::{
        devfs::devfs_init,
        fat::fs::FATFileSystem,
        procfs::procfs_init,
        ramfs::RamFS,
        sysfs::sysfs_init,
        vfs::{mount::MountFS, syscall::ModeType, AtomicInodeId, FileSystem, FileType},
    },
    kdebug, kerror, kinfo,
    process::ProcessManager,
};

use super::{
    file::FileMode,
    utils::{rsplit_path, user_path_at},
    IndexNode, InodeId, MAX_PATHLEN, VFS_MAX_FOLLOW_SYMLINK_TIMES,
};

/// @brief 原子地生成新的Inode号。
/// 请注意，所有的inode号都需要通过该函数来生成.全局的inode号，除了以下两个特殊的以外，都是唯一的
/// 特殊的两个inode号：
/// [0]: 对应'.'目录项
/// [1]: 对应'..'目录项
pub fn generate_inode_id() -> InodeId {
    static INO: AtomicInodeId = AtomicInodeId::new(InodeId::new(1));
    return INO.fetch_add(InodeId::new(1), Ordering::SeqCst);
}

static mut __ROOT_INODE: Option<Arc<dyn IndexNode>> = None;

/// @brief 获取全局的根节点
#[inline(always)]
#[allow(non_snake_case)]
pub fn ROOT_INODE() -> Arc<dyn IndexNode> {
    unsafe {
        return __ROOT_INODE.as_ref().unwrap().clone();
    }
}

/// 初始化虚拟文件系统
#[inline(never)]
pub fn vfs_init() -> Result<(), SystemError> {
    // 使用Ramfs作为默认的根文件系统
    let ramfs = RamFS::new();
    let mount_fs = MountFS::new(ramfs, None);
    let root_inode = mount_fs.root_inode();

    unsafe {
        __ROOT_INODE = Some(root_inode.clone());
    }

    // 创建文件夹
    root_inode
        .create("proc", FileType::Dir, ModeType::from_bits_truncate(0o755))
        .expect("Failed to create /proc");
    root_inode
        .create("dev", FileType::Dir, ModeType::from_bits_truncate(0o755))
        .expect("Failed to create /dev");
    root_inode
        .create("sys", FileType::Dir, ModeType::from_bits_truncate(0o755))
        .expect("Failed to create /sys");
    kdebug!("dir in root:{:?}", root_inode.list());

    procfs_init().expect("Failed to initialize procfs");

    devfs_init().expect("Failed to initialize devfs");

    sysfs_init().expect("Failed to initialize sysfs");

    let root_entries = ROOT_INODE().list().expect("VFS init failed");
    if root_entries.len() > 0 {
        kinfo!("Successfully initialized VFS!");
    }
    return Ok(());
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
            .create(
                mountpoint_name,
                FileType::Dir,
                ModeType::from_bits_truncate(0o755),
            )
            .expect(format!("Failed to create '/{mountpoint_name}' in migrating").as_str())
    } else {
        r.unwrap()
    };
    // 迁移挂载点
    mountpoint
        .mount(fs.inner_filesystem())
        .expect(format!("Failed to migrate {mountpoint_name} ").as_str());
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
    let new_root_inode = new_fs.root_inode();

    // 把上述文件系统,迁移到新的文件系统下
    do_migrate(new_root_inode.clone(), "proc", proc)?;
    do_migrate(new_root_inode.clone(), "dev", dev)?;
    do_migrate(new_root_inode.clone(), "sys", sys)?;
    unsafe {
        // drop旧的Root inode
        let old_root_inode = __ROOT_INODE.take().unwrap();
        drop(old_root_inode);

        // 设置全局的新的ROOT Inode
        __ROOT_INODE = Some(new_root_inode);
    }

    kinfo!("VFS: Migrate filesystems done!");

    return Ok(());
}

pub fn mount_root_fs() -> Result<(), SystemError> {
    kinfo!("Try to mount FAT32 as root fs...");
    let partiton: Arc<Partition> = ahci::get_disks_by_name("ahci_disk_0".to_string())
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

    return Ok(());
}

/// @brief 创建文件/文件夹
pub fn do_mkdir(path: &str, _mode: FileMode) -> Result<u64, SystemError> {
    // 文件名过长
    if path.len() > MAX_PATHLEN as usize {
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
            let _create_inode: Arc<dyn IndexNode> = parent_inode.create(
                filename,
                FileType::Dir,
                ModeType::from_bits_truncate(0o755),
            )?;
        } else {
            // 不需要创建文件，因此返回错误码
            return Err(errno);
        }
    }

    return Ok(0);
}

/// @brief 删除文件夹
pub fn do_remove_dir(dirfd: i32, path: &str) -> Result<u64, SystemError> {
    // 文件名过长
    if path.len() > MAX_PATHLEN as usize {
        return Err(SystemError::ENAMETOOLONG);
    }

    let pcb = ProcessManager::current_pcb();
    let (inode_begin, remain_path) = user_path_at(&pcb, dirfd, path)?;
    let (filename, parent_path) = rsplit_path(&remain_path);

    // 最后一项文件项为.时返回EINVAL
    if filename == "." {
        return Err(SystemError::EINVAL);
    }

    // 查找父目录
    let parent_inode: Arc<dyn IndexNode> = inode_begin
        .lookup_follow_symlink(parent_path.unwrap_or("/"), VFS_MAX_FOLLOW_SYMLINK_TIMES)?;

    if parent_inode.metadata()?.file_type != FileType::Dir {
        return Err(SystemError::ENOTDIR);
    }

    // 在目标点为symlink时也返回ENOTDIR
    let target_inode = parent_inode.find(filename)?;
    if target_inode.metadata()?.file_type != FileType::Dir {
        return Err(SystemError::ENOTDIR);
    }

    // 删除文件夹
    parent_inode.rmdir(filename)?;

    return Ok(0);
}

/// @brief 删除文件
pub fn do_unlink_at(dirfd: i32, path: &str) -> Result<u64, SystemError> {
    // 文件名过长
    if path.len() > MAX_PATHLEN as usize {
        return Err(SystemError::ENAMETOOLONG);
    }
    let pcb = ProcessManager::current_pcb();
    let (inode_begin, remain_path) = user_path_at(&pcb, dirfd, path)?;

    let inode: Result<Arc<dyn IndexNode>, SystemError> =
        inode_begin.lookup_follow_symlink(&remain_path, VFS_MAX_FOLLOW_SYMLINK_TIMES);

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
    let parent_inode: Arc<dyn IndexNode> = inode_begin
        .lookup_follow_symlink(parent_path.unwrap_or("/"), VFS_MAX_FOLLOW_SYMLINK_TIMES)?;

    if parent_inode.metadata()?.file_type != FileType::Dir {
        return Err(SystemError::ENOTDIR);
    }

    // 删除文件
    parent_inode.unlink(filename)?;

    return Ok(0);
}
