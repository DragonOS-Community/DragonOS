use alloc::string::String;
use alloc::string::ToString;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::filesystem::ramfs::RamFS;
use crate::filesystem::vfs::mount::MountFlags;
use crate::filesystem::vfs::FilePrivateData;
use crate::filesystem::vfs::FileSystem;
use crate::filesystem::vfs::MountFS;
use crate::init::boot::boot_callbacks;
use crate::init::initcall::INITCALL_ROOTFS;
use crate::libs::decompress::xz_decompress;
use crate::libs::spinlock::SpinLock;
use crate::process::namespace::propagation::MountPropagation;
use cpio_reader::Mode;
use system_error::SystemError;
use unified_init::macros::unified_init;

use crate::filesystem::vfs::{syscall::ModeType, utils::rsplit_path, FileType, IndexNode};

static mut __INIT_ROOT_INODE: Option<Arc<dyn IndexNode>> = None;

pub static mut __INIT_ROOT_ENABLED: bool = false;

/// @brief 获取全局的 Initramfs 根节点
#[inline(always)]
#[allow(non_snake_case)]
pub fn INIT_ROOT_INODE() -> Arc<dyn IndexNode> {
    unsafe {
        return __INIT_ROOT_INODE.as_ref().unwrap().clone();
    }
}

#[allow(non_upper_case_globals, unexpected_cfgs)]
#[used]
pub static INITRAM_DATA: &[u8] = {
    #[cfg(has_initram)]
    {
        include_bytes!(env!("INITRAM_PATH"))
    }
    #[cfg(not(has_initram))]
    {
        &[]
    }
};

/// 获取内核中 initramfs 的数据的起始地址
pub fn get_initramfs_start_addr() -> usize {
    INITRAM_DATA.as_ptr() as usize
}

/// 获取内核中 initramfs 的数据的 Size
pub fn get_initramfs_size() -> usize {
    INITRAM_DATA.len()
}

/// 获取 initramfs 的数据的全新 Vec
/// 此函数会复制内核中包含的 initramfs 内容到一个新的 Vec 中
#[allow(dead_code)]
pub fn get_initram_data() -> Vec<u8> {
    INITRAM_DATA.to_vec()
}

/// 获取 initramfs 的数据的 Vec 引用
/// 此函数会返回内核中包含的 initramfs 内容的引用
pub fn get_initram() -> &'static [u8] {
    INITRAM_DATA
}

#[derive(PartialEq, Eq, Hash, Debug, Clone)]
struct CpioEntryInfo {
    name: String,
    file: Vec<u8>,
    mode: Mode,
    uid: u32,
    gid: u32,
    ino: u32,
    mtime: u64,
    nlink: u32,
    dev: Option<u32>,
    devmajor: Option<u32>,
    devminor: Option<u32>,
    rdev: Option<u32>,
    rdevmajor: Option<u32>,
    rdevminor: Option<u32>,
}

/// 目前只支持内核嵌入 xz 压缩格式的文件，他是使用命令"xz --check=crc32 --lzma2=dict=512KiB /tmp/initramfs.linux_amd64.cpio"得到的
/// 同时对 cpio 格式的支持请见 cpio_reader crate
/// 参考文献：https://book.linuxboot.org/coreboot.u-root.systemboot/index.html
#[unified_init(INITCALL_ROOTFS)]
#[inline(never)]
pub fn initramfs_init() -> Result<(), SystemError> {
    log::info!("initramfs_init start");

    let ramfs = RamFS::new();
    let mount_fs = MountFS::new(
        ramfs,
        None,
        MountPropagation::new_private(),
        None,
        MountFlags::empty(),
    );
    let root_inode = mount_fs.root_inode();
    unsafe {
        __INIT_ROOT_INODE = Some(root_inode.clone());
    }

    // Linux 中，内嵌的 initramfs 始终存在
    // 最新 Linux 使用编译参数控制是否包含和开启
    log::info!(
        "Inner initramfs(Compressed file) start addr is {:#x}, size is {:#x}",
        get_initramfs_start_addr(),
        get_initramfs_size()
    );

    if get_initramfs_size() == 0 {
        log::error!("Initramfs error: Not found initram, the size is 0!");
        return Err(SystemError::ENOENT);
    }

    let cpio_data = xz_decompress(get_initram())?;

    let collected_entries_vec = cpio_reader::iter_files(&cpio_data)
        .map(|entry| CpioEntryInfo {
            name: entry.name().to_string(),
            file: entry.file().to_vec(),
            mode: entry.mode(),
            uid: entry.uid(),
            gid: entry.gid(),
            ino: entry.ino(),
            mtime: entry.mtime(),
            nlink: entry.nlink(),
            dev: entry.dev(),
            devmajor: entry.devmajor(),
            devminor: entry.devminor(),
            rdev: entry.rdev(),
            rdevmajor: entry.rdevmajor(),
            rdevminor: entry.rdevminor(),
        })
        .collect::<Vec<_>>();

    let mut links: Vec<usize> = Vec::new();

    for (index, entry) in collected_entries_vec.iter().enumerate() {
        // x86 的有 4 种文件：Dir, File, CharDevice, SymLink
        let name = entry.name.clone();
        let mode = ModeType::from_bits(entry.mode.bits()).ok_or_else(|| {
            log::error!("initramfs: failed to get mode!");
            SystemError::EINVAL
        })?;
        let file_type = FileType::from(mode);
        log::info!(
            "Find cpio entry, Name:{}, ModeType:{:?}, FileType:{:?}",
            name,
            mode,
            file_type
        );
        let (filename, parent_path) = rsplit_path(&name);
        let parent_inode = match parent_path {
            None => INIT_ROOT_INODE(),
            Some(path) => INIT_ROOT_INODE().lookup(path)?,
        };
        match file_type {
            FileType::Dir => {
                // 直接插入, 无需处理数据
                parent_inode.create(filename, file_type, mode)?;
            }
            FileType::File => {
                // 插入, 随后写入文件数据
                let inode = parent_inode.create(filename, file_type, mode)?;
                inode.write_at(
                    0,
                    entry.file.len(),
                    &entry.file,
                    SpinLock::new(crate::filesystem::vfs::FilePrivateData::Unused).lock(),
                )?;
            }
            FileType::CharDevice => {
                // 不处理, 如果使用 initramfs 那么直接从已经初始化好的根文件系统迁移到此文件系统
            }
            FileType::SymLink => {
                // 暂时标记存入, 当 Dir 和 File 全部创建完成之后再创建链接, 因为有可能先读取到链接文件
                links.push(index);
            }
            _ => {
                panic!("FileType is not impled!");
            }
        };
    }

    // 处理链接文件, 使用符号链接
    for i in 0..links.len() {
        let entry = &collected_entries_vec[links[i]];
        let name = entry.name.clone();
        let (filename, parent_path) = rsplit_path(&name);
        let parent_inode = match parent_path {
            None => INIT_ROOT_INODE(),
            Some(path) => INIT_ROOT_INODE().lookup(path)?,
        };
        let other_name = String::from_utf8(entry.file.clone()).map_err(|err| {
            log::error!("initramfs: failed to get utf8_name, err is {}", err);
            SystemError::EINVAL
        })?;
        let new_inode =
            parent_inode.create_with_data(filename, FileType::SymLink, ModeType::S_IRWXUGO, 0)?;
        let buf = other_name.as_bytes();
        let len = buf.len();
        new_inode.write_at(0, len, buf, SpinLock::new(FilePrivateData::Unused).lock())?;
    }

    // 下面的方式是查看外置 initramfs, 例如使用 qemu 的 -initrd 参数加载的
    // 这个是从 bios 传过来的 bootinfo 查找由 bios 加载到内存的 initramfs
    // 暂时没实现，待实现
    // 实现后需要参照 Linux 对内嵌和外置同时存在时 rootfs 的处理进行覆盖
    // https://docs.linuxkernel.org.cn/filesystems/ramfs-rootfs-initramfs.html
    boot_callbacks()
        .init_initramfs()
        .inspect_err(|e| {
            log::error!("Failed to init boot initramfs: {:?}", e);
        })
        .ok();

    // 检查是否使用 initramfs 作为根文件系统启动
    // 判断标准: 是否存在 /init 程序, 与 Linux 相同
    // 查找考虑链接
    unsafe {
        __INIT_ROOT_ENABLED = INIT_ROOT_INODE().find("init").is_ok();
        if !__INIT_ROOT_ENABLED {
            // TODO: drop 掉所有的资源
            // 此分支未做测试, 可能有内存释放不完全
            let old_root_inode = __INIT_ROOT_INODE.take().unwrap();
            drop(old_root_inode);
            log::info!("Rootfs: will not use initramfs");
            log::info!("initramfs_init done!");
            return Ok(());
        }
    }

    // 清除 dev, proc, sys 三个文件夹, 后续直接迁移根文件系统的过来
    // 这里是因为 linux 默认不挂载这些文件夹, 通常交给 init 程序完成, 但是 DragonOS 会默认挂载
    INIT_ROOT_INODE()
        .rmdir("dev")
        .expect("initramfs: Unable to remove /dev");
    INIT_ROOT_INODE()
        .rmdir("proc")
        .expect("initramfs: Unable to remove /proc");
    INIT_ROOT_INODE()
        .rmdir("sys")
        .expect("initramfs: Unable to remove /sys");

    log::info!("initramfs_init done!");
    Ok(())
}
