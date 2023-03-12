use core::{
    hint::spin_loop,
    ptr::null_mut,
    sync::atomic::{AtomicUsize, Ordering},
};

use alloc::{boxed::Box, format, string::ToString, sync::Arc};

use crate::{
    arch::asm::current::current_pcb,
    driver::disk::ahci::{self},
    filesystem::{
        devfs::DevFS,
        fat::fs::FATFileSystem,
        procfs::ProcFS,
        ramfs::RamFS,
        vfs::{file::File, mount::MountFS, FileSystem, FileType},
    },
    include::bindings::bindings::{EBADF, ENAMETOOLONG, ENOENT, ENOTDIR, EPERM, PAGE_4K_SIZE},
    io::SeekFrom,
    kerror, kinfo,
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
) -> Result<(), i32> {
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
fn migrate_virtual_filesystem(new_fs: Arc<dyn FileSystem>) -> Result<(), i32> {
    kinfo!("VFS: Migrating filesystems...");

    // ==== 在这里获取要被迁移的文件系统的inode ===
    let binding = ROOT_INODE().find("proc").expect("ProcFS not mounted!").fs();
    let proc: &MountFS = binding.as_any_ref().downcast_ref::<MountFS>().unwrap();
    let binding = ROOT_INODE().find("dev").expect("DevFS not mounted!").fs();
    let dev: &MountFS = binding.as_any_ref().downcast_ref::<MountFS>().unwrap();

    let new_fs = MountFS::new(new_fs, None);
    // 获取新的根文件系统的根节点的引用
    let new_root_inode = Box::leak(Box::new(new_fs.root_inode()));

    // 把上述文件系统,迁移到新的文件系统下
    do_migrate(new_root_inode.clone(), "proc", proc)?;
    do_migrate(new_root_inode.clone(), "dev", dev)?;

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
    let partiton: Arc<crate::io::disk_info::Partition> =
        ahci::get_disks_by_name("ahci_disk_0".to_string())
            .unwrap()
            .0
            .lock()
            .partitions[0]
            .clone();

    let fatfs: Result<Arc<FATFileSystem>, i32> = FATFileSystem::new(partiton);
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

/// @brief 为当前进程打开一个文件
pub fn do_open(path: &str, mode: FileMode) -> Result<i32, i32> {
    // 文件名过长
    if path.len() > PAGE_4K_SIZE as usize {
        return Err(-(ENAMETOOLONG as i32));
    }

    let inode: Result<Arc<dyn IndexNode>, i32> = ROOT_INODE().lookup(path);

    let inode: Arc<dyn IndexNode> = if inode.is_err() {
        let errno = inode.unwrap_err();
        // 文件不存在，且需要创建
        if mode.contains(FileMode::O_CREAT)
            && !mode.contains(FileMode::O_DIRECTORY)
            && errno == -(ENOENT as i32)
        {
            let (filename, parent_path) = rsplit_path(path);
            // 查找父目录
            let parent_inode: Arc<dyn IndexNode> =
                ROOT_INODE().lookup(parent_path.unwrap_or("/"))?;
            // 创建文件
            let inode: Arc<dyn IndexNode> = parent_inode.create(filename, FileType::File, 0o777)?;
            inode
        } else {
            // 不需要创建文件，因此返回错误码
            return Err(errno);
        }
    } else {
        inode.unwrap()
    };

    let file_type: FileType = inode.metadata()?.file_type;
    // 如果要打开的是文件夹，而目标不是文件夹
    if mode.contains(FileMode::O_DIRECTORY) && file_type != FileType::Dir {
        return Err(-(ENOTDIR as i32));
    }

    // 如果O_TRUNC，并且，打开模式包含O_RDWR或O_WRONLY，清空文件
    if mode.contains(FileMode::O_TRUNC)
        && (mode.contains(FileMode::O_RDWR) || mode.contains(FileMode::O_WRONLY))
        && file_type == FileType::File
    {
        inode.truncate(0)?;
    }

    // 创建文件对象
    let mut file: File = File::new(inode, mode)?;

    // 打开模式为“追加”
    if mode.contains(FileMode::O_APPEND) {
        file.lseek(SeekFrom::SeekEnd(0))?;
    }

    // 把文件对象存入pcb
    return current_pcb().alloc_fd(file);
}

/// @brief 根据文件描述符，读取文件数据。尝试读取的数据长度与buf的长度相同。
///
/// @param fd 文件描述符编号
/// @param buf 输出缓冲区。
///
/// @return Ok(usize) 成功读取的数据的字节数
/// @return Err(i32) 读取失败，返回posix错误码
pub fn do_read(fd: i32, buf: &mut [u8]) -> Result<usize, i32> {
    let file: Option<&mut File> = current_pcb().get_file_mut_by_fd(fd);
    if file.is_none() {
        return Err(-(EBADF as i32));
    }
    let file: &mut File = file.unwrap();

    return file.read(buf.len(), buf);
}

/// @brief 根据文件描述符，向文件写入数据。尝试写入的数据长度与buf的长度相同。
///
/// @param fd 文件描述符编号
/// @param buf 输入缓冲区。
///
/// @return Ok(usize) 成功写入的数据的字节数
/// @return Err(i32) 写入失败，返回posix错误码
pub fn do_write(fd: i32, buf: &[u8]) -> Result<usize, i32> {
    let file: Option<&mut File> = current_pcb().get_file_mut_by_fd(fd);
    if file.is_none() {
        return Err(-(EBADF as i32));
    }
    let file: &mut File = file.unwrap();

    return file.write(buf.len(), buf);
}

/// @brief 调整文件操作指针的位置
///
/// @param fd 文件描述符编号
/// @param seek 调整的方式
///
/// @return Ok(usize) 调整后，文件访问指针相对于文件头部的偏移量
/// @return Err(i32) 调整失败，返回posix错误码
pub fn do_lseek(fd: i32, seek: SeekFrom) -> Result<usize, i32> {
    let file: Option<&mut File> = current_pcb().get_file_mut_by_fd(fd);
    if file.is_none() {
        return Err(-(EBADF as i32));
    }
    let file: &mut File = file.unwrap();
    return file.lseek(seek);
}

/// @brief 创建文件/文件夹
pub fn do_mkdir(path: &str, _mode: FileMode) -> Result<u64, i32> {
    // 文件名过长
    if path.len() > PAGE_4K_SIZE as usize {
        return Err(-(ENAMETOOLONG as i32));
    }

    let inode: Result<Arc<dyn IndexNode>, i32> = ROOT_INODE().lookup(path);

    if inode.is_err() {
        let errno = inode.unwrap_err();
        // 文件不存在，且需要创建
        if errno == -(ENOENT as i32) {
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

/// @breif 删除文件夹
pub fn do_remove_dir(path: &str) -> Result<u64, i32> {
    // 文件名过长
    if path.len() > PAGE_4K_SIZE as usize {
        return Err(-(ENAMETOOLONG as i32));
    }

    let inode: Result<Arc<dyn IndexNode>, i32> = ROOT_INODE().lookup(path);

    if inode.is_err() {
        let errno = inode.unwrap_err();
        // 文件不存在
        if errno == -(ENOENT as i32) {
            return Err(-(ENOENT as i32));
        }
    }

    let (filename, parent_path) = rsplit_path(path);
    // 查找父目录
    let parent_inode: Arc<dyn IndexNode> = ROOT_INODE().lookup(parent_path.unwrap_or("/"))?;

    if parent_inode.metadata()?.file_type != FileType::Dir {
        return Err(-(ENOTDIR as i32));
    }

    let target_inode: Arc<dyn IndexNode> = parent_inode.find(filename)?;
    if target_inode.metadata()?.file_type != FileType::Dir {
        return Err(-(ENOTDIR as i32));
    }

    // 删除文件夹
    parent_inode.rmdir(filename)?;

    return Ok(0);
}

/// @brief 删除文件
pub fn do_unlink_at(path: &str, _mode: FileMode) -> Result<u64, i32> {
    // 文件名过长
    if path.len() > PAGE_4K_SIZE as usize {
        return Err(-(ENAMETOOLONG as i32));
    }

    let inode: Result<Arc<dyn IndexNode>, i32> = ROOT_INODE().lookup(path);

    if inode.is_err() {
        let errno = inode.clone().unwrap_err();
        // 文件不存在，且需要创建
        if errno == -(ENOENT as i32) {
            return Err(-(ENOENT as i32));
        }
    }
    // 禁止在目录上unlink
    if inode.unwrap().metadata()?.file_type == FileType::Dir {
        return Err(-(EPERM as i32));
    }

    let (filename, parent_path) = rsplit_path(path);
    // 查找父目录
    let parent_inode: Arc<dyn IndexNode> = ROOT_INODE().lookup(parent_path.unwrap_or("/"))?;

    if parent_inode.metadata()?.file_type != FileType::Dir {
        return Err(-(ENOTDIR as i32));
    }

    // 删除文件
    parent_inode.unlink(filename)?;

    return Ok(0);
}
