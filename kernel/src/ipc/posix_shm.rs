//! # POSIX 共享内存支持
//! 
//! POSIX 共享内存通过 /dev/shm tmpfs 实现。
//! shm_open/shm_unlink 在用户空间库中实现，调用标准的 open/unlink 系统调用。

use alloc::string::ToString;
use system_error::SystemError;
use crate::filesystem::vfs::syscall::ModeType;

/// 创建并挂载 /dev/shm 目录
pub fn setup_dev_shm() -> Result<(), SystemError> {
    use crate::filesystem::vfs::syscall::sys_mount::do_mount;
    
    // 获取根inode
    let root_inode = crate::process::ProcessManager::current_mntns().root_inode();
    // 在 /dev 下创建 shm 目录
    let dev_inode = root_inode.find("dev")?;
    
    // 检查 shm 目录是否已存在
    if dev_inode.find("shm").is_err() {
        // 创建 shm 目录
        dev_inode.create("shm", crate::filesystem::vfs::FileType::Dir, 
                        ModeType::from_bits_truncate(0o755))?;
        log::info!("Created /dev/shm directory");
    }
    
    // 挂载 tmpfs 到 /dev/shm
    let shm_path = "/dev/shm";
    log::info!("Attempting to mount tmpfs on {}", shm_path);
    
    let result = do_mount(
        Some("tmpfs".to_string()),  // source - tmpfs 需要一个 source
        Some(shm_path.to_string()), // target
        Some("tmpfs".to_string()),  // filesystem type
        None,                       // data
        crate::filesystem::vfs::mount::MountFlags::empty(), // flags
    );
    
    match result {
        Ok(_) => {
            log::info!("Successfully mounted tmpfs on /dev/shm");
            Ok(())
        }
        Err(e) => {
            log::warn!("Failed to mount tmpfs on /dev/shm: {:?}", e);
            // 即使挂载失败，也不应该阻止系统启动
            Ok(())
        }
    }
}

/// 初始化POSIX共享内存支持（仅挂载 /dev/shm）
pub fn init_posix_shm() -> Result<(), SystemError> {
    setup_dev_shm()?;
    log::info!("POSIX shared memory support initialized (/dev/shm mounted)");
    Ok(())
}
