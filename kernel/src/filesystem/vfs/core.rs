use core::{
    any::Any,
    sync::atomic::{AtomicUsize, Ordering},
};

use alloc::{sync::Arc, vec::Vec, string::String, format};

use crate::{
    filesystem::{
        procfs::{LockedProcFSInode, ProcFS},
        ramfs::RamFS,
        vfs::{
            mount::MountFS,
            FileSystem, FileType, file::File,
        },
    },
    kdebug, println, include::bindings::bindings::{O_RDWR, O_RDONLY},
};

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
        
        // 创建文件夹
        root_inode.create("proc", FileType::Dir, 0o777).expect("Failed to create /proc");
        root_inode.create("dev", FileType::Dir, 0o777).expect("Failed to create /dev");
        // 创建procfs实例
        let procfs = ProcFS::new();
        kdebug!("proc created");
        kdebug!("root inode.list()={:?}", root_inode.list());
        // procfs挂载
        let _t = root_inode.find("proc").expect("Cannot find /proc").mount(procfs).expect("Failed to mount procfs.");
        kdebug!("root inode.list()={:?}", root_inode.list());
        root_inode
    };
}
pub fn print_type_of<T>(_: &T) {
    println!("{}", core::any::type_name::<T>())
}

/// @brief 在这个函数里面，编写调试文件系统用的代码。该函数仅供重构期间，方便调试使用。
///
/// 建议在这个函数里面，调用其他的调试函数。（避免merge的时候出现大量冲突）
pub fn __test_filesystem() {
    __test_rootfs();
}

fn __test_rootfs() {
    kdebug!("root inode.list()={:?}", ROOT_INODE.list());
}

fn __as_any_ref<T: Any>(x: &T) -> &dyn core::any::Any {
    x
}

/// @brief procfs测试函数
pub fn _test_procfs(pid: i64) {
    __test_procfs(pid);
}

fn __test_procfs(pid: i64) {
    kdebug!("to register pid: {}", pid);
    // 获取procfs实例
    let _p = ROOT_INODE.find("proc").unwrap();

    let procfs_inode = _p.downcast_ref::<LockedProcFSInode>().unwrap();
    let fs = procfs_inode.fs();
    let fs = fs.as_any_ref().downcast_ref::<ProcFS>().unwrap();
    kdebug!("to procfs_register_pid");
    // 调用注册函数
    fs.procfs_register_pid(pid).expect("register pid failed");
    // /proc/1/status
    kdebug!("procfs_register_pid ok");
    kdebug!(
        "root inode.list()={:?}",
        ROOT_INODE.list().expect("list / failed.")
    );
    // let proc_inode = ROOT_INODE.lookup("/proc/1/status").expect("Cannot find /proc/1/status");
    let _t = procfs_inode.find(&format!("{}", pid)).unwrap();
    let proc_inode = _t.find("status").expect(&format!("Cannot find /proc/{}/status", pid));
    let mut f = File::new(proc_inode, O_RDONLY).unwrap();
    kdebug!("file created!");
    kdebug!(
        "proc.list()={:?}",
        _p.list().expect("list /proc failed.")
    );
    let mut buf : Vec<u8> = Vec::new();
    buf.resize(f.metadata().unwrap().size as usize, 0);

    let size = f.read(
        f.metadata().unwrap().size as usize, buf.as_mut()).unwrap();
    kdebug!("size = {}, data={:?}", size, buf);
    let buf = String::from_utf8(buf).unwrap();
    kdebug!("data for /proc/{}/status: {}", pid, buf);

}
