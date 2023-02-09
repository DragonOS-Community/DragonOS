use core::sync::atomic::{AtomicUsize, Ordering};

use alloc::sync::Arc;

use crate::{filesystem::{
    ramfs::RamFS,
    vfs::{mount::{MountFS, MountFSInode}, FileSystem, FileType}, procfs::ProcFS,
}, kdebug};

use super::{IndexNode, InodeId};

/// @brief 原子地生成新的Inode号。
/// 请注意，所有的inode号都需要通过该函数来生成.全局的inode号，除了以下两个特殊的以外，都是唯一的
/// 特殊的两个inode号：
/// [0]: 对应'.'目录项
/// [1]: 对应'..'目录项
pub fn generate_inode_id() -> InodeId {
    static INO: AtomicUsize = AtomicUsize::new(1);
    return INO.fetch_add(1, Ordering::SeqCst);
}

// @brief 初始化ROOT INODE
lazy_static! {
    pub static ref ROOT_INODE: Arc<dyn IndexNode> = {
        // 使用Ramfs作为默认的根文件系统
        let ramfs = RamFS::new();
        let rootfs = MountFS::new(ramfs, None);
        let root_inode = rootfs.get_root_inode();

        // 创建procfs实例
        let procfs = ProcFS::new();
        let proc_fs = MountFS::new(procfs, None);

        // 创建文件夹 
        root_inode.create("proc", FileType::Dir, 0o777).expect("Failed to create /proc");
        root_inode.create("dev", FileType::Dir, 0o777).expect("Failed to create /dev");
        // procfs mount
        let _t = root_inode.find("proc").unwrap().as_any_ref().downcast_ref::<MountFSInode>().unwrap().mount(proc_fs);
        root_inode
    };
}


/// @brief 在这个函数里面，编写调试文件系统用的代码。该函数仅供重构期间，方便调试使用。
/// 
/// 建议在这个函数里面，调用其他的调试函数。（避免merge的时候出现大量冲突）
pub fn __test_filesystem(){
        __test_rootfs();
}

/// @brief procfs测试函数
pub fn _test_procfs(pid: i64){
    __test_procfs(pid);
}

fn __test_rootfs(){
    kdebug!("root inode.list()={:?}", ROOT_INODE.list());
}

fn __test_procfs(pid: i64){
    // 获取procfs实例
    let _p = ROOT_INODE.find("proc").unwrap();
    // 调用注册函数
    // _p.procfs_register_pid(pid);
    // 
}