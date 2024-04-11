use core::{hint::spin_loop, sync::atomic::Ordering};

use alloc::{string::ToString, sync::Arc};
use system_error::SystemError;

use crate::{
    driver::{base::block::disk_info::Partition, disk::ahci},
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
    file::FileMode, mount::{MountFSInode, MountList}, open::do_sys_open, syscall::UmountFlag, utils::{rsplit_path, user_path_at}, IndexNode, InodeId, VFS_MAX_FOLLOW_SYMLINK_TIMES
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
    if !root_entries.is_empty() {
        kinfo!("Successfully initialized VFS!");
    }
    return Ok(());
}

/// @brief 真正执行伪文件系统迁移的过程
///
/// @param mountpoint_name 在根目录下的挂载点的名称
/// @param inode 原本的挂载点的inode
fn do_migrate(
    new_root_inode: &Arc<dyn IndexNode>,
    mountpoint_name: &str,
    fs: Arc<dyn FileSystem>,
) -> Result<(), SystemError> {
    new_root_inode
        .find(mountpoint_name)
        .unwrap_or(
        new_root_inode
            .create(
                mountpoint_name,
                FileType::Dir,
                ModeType::from_bits_truncate(0o755),
            )
            .unwrap_or_else(|_| panic!("Failed to create '/{mountpoint_name}' in migrating"))
    );

    // 迁移挂载点
    // let inode = mountpoint.arc_any().downcast::<MountFSInode>().unwrap();
    // inode.do_mount(inode.inode_id(), fs.self_ref())?;
    // mountpoint.mount(fs.clone())?;
    do_mount(fs, mountpoint_name)?;

    return Ok(());
}

/// @brief 迁移伪文件系统的inode
/// 请注意，为了避免删掉了伪文件系统内的信息，因此没有在原root inode那里调用unlink.
fn migrate_virtual_filesystem(new_fs: Arc<dyn FileSystem>) -> Result<(), SystemError> {
    kinfo!("VFS: Migrating filesystems...");
    
    let new_fs = MountFS::new(new_fs, None);
    // 获取新的根文件系统的根节点的引用
    let new_root_inode = new_fs.root_inode();

    unsafe {
        // drop旧的Root inode
        let old_root_inode = __ROOT_INODE.take().unwrap();

        // ==== 在这里获取要被迁移的文件系统的inode ===
        let proc = old_root_inode.find("proc").expect("ProcFS not mounted!").fs();
        let dev = old_root_inode.find("dev").expect("DevFS not mounted!").fs();
        let sys = old_root_inode.find("sys").expect("SysFs not mounted!").fs();

        // 设置全局的新的ROOT Inode
        __ROOT_INODE = Some(new_root_inode.clone());

        // 把上述文件系统,迁移到新的文件系统下
        do_migrate(&new_root_inode, "/proc", proc)?;
        do_migrate(&new_root_inode, "/dev", dev)?;
        do_migrate(&new_root_inode, "/sys", sys)?;
    
        drop(old_root_inode);
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
    let path = path.trim();

    let inode: Result<Arc<dyn IndexNode>, SystemError> = ROOT_INODE().lookup(path);

    if let Err(errno) = inode {
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
    let path = path.trim();

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
    let path = path.trim();

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

// @brief mount filesystem
/// 总是应该从此处挂载文件系统
pub fn do_mount(fs: Arc<dyn FileSystem>, mount_point: &str) -> Result<usize, SystemError> {
    let fs = ROOT_INODE()
        .lookup_follow_symlink(mount_point, VFS_MAX_FOLLOW_SYMLINK_TIMES)?
        .mount(fs)?;
    MountList::insert(mount_point, fs);
    Ok(0)
}

/// 总是应该从此处卸载文件系统
pub fn do_umount2(dirfd: i32, target: &str, _flag: UmountFlag) -> Result<Arc<MountFS>, SystemError> {
    let (work, rest) = user_path_at(&ProcessManager::current_pcb(), dirfd, &target)?;
    let path = work.absolute_path(0)? + &rest;
    let do_umount = || -> Result<Arc<MountFS>, SystemError> {
        if let Some(fs) = MountList::remove(path) {
            // Todo: 占用检测
            // kdebug!("Umount Target found!");
            fs.umount()?;
            return Ok(fs);
        }
        return Err(SystemError::EINVAL);
    };
    return do_umount();
}