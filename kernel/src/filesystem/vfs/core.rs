use core::{
    any::Any,
    hint::spin_loop,
    sync::atomic::{compiler_fence, AtomicUsize, Ordering},
};

use alloc::{
    format,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};

use crate::{
    driver::disk::ahci::{self, ahci_rust_init},
    filesystem::{
        fat::fs::FATFileSystem,
        procfs::{LockedProcFSInode, ProcFS},
        ramfs::RamFS,
        vfs::{file::File, mount::MountFS, FileSystem, FileType},
    },
    include::bindings::bindings::{O_RDONLY, O_RDWR},
    kdebug, kerror, print, println,
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
        ahci_rust_init().expect("ahci rust init failed.");
        // 使用Ramfs作为默认的根文件系统
        let ramfs = RamFS::new();
        let rootfs = MountFS::new(ramfs, None);
        let root_inode = rootfs.root_inode();
        
        test_fatfs();
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

fn test_fatfs() {
    let partiton: Arc<crate::io::disk_info::Partition> =
        ahci::get_disks_by_name("ahci_disk_0".to_string())
            .unwrap()
            .0
            .lock()
            .partitions[0]
            .clone();

    // ========== 测试挂载文件系统 ===============
    let fatfs: Result<Arc<FATFileSystem>, i32> = FATFileSystem::new(partiton);
    if fatfs.is_err() {
        kerror!(
            "Failed to initialize fatfs, code={:?}",
            fatfs.as_ref().err()
        );
    }
    let fatfs = fatfs.unwrap();

    // ======= 测试读取目录 =============
    let fat_root = fatfs.root_inode();
    kdebug!("get fat root inode ok");
    let root_items: Result<Vec<String>, i32> = fat_root.list();
    kdebug!("list root inode = {:?}", root_items);
    if root_items.is_ok() {
        let root_items: Vec<String> = root_items.unwrap();
        kdebug!("root items = {:?}", root_items);
    } else {
        kerror!("list root_items failed, code = {}", root_items.unwrap_err());
    }
    // kdebug!("to find boot");
    // let boot_inode = fat_root.find("boot").unwrap();
    // kdebug!("to list boot");
    // kdebug!("boot items = {:?}", boot_inode.list().unwrap());
    // kdebug!("to find grub");
    // let grub_inode = boot_inode.find("grub").unwrap();
    // kdebug!("to list grub");
    // kdebug!("grub_items={:?}", grub_inode.list().unwrap());
    // let grub_cfg_inode = grub_inode.find("grub.cfg").unwrap();

    // // ========== 测试读写 =============
    // kdebug!("grub_cfg_inode = {:?}", grub_cfg_inode);
    // let mut buf: Vec<u8> = Vec::new();
    // buf.resize(128, 0);
    // let mut file = File::new(grub_cfg_inode, O_RDWR).unwrap();
    // kdebug!("file={file:?}, metadata = {:?}", file.metadata());
    // let r = file.read(128, &mut buf);
    // kdebug!("r = {r:?}, buf={buf:?}");
    // for x in buf.iter() {
    //     print!("{}", *x as char);
    // }
    // buf[126] = "X".as_bytes()[0];
    // buf[127] = "X".as_bytes()[0];
    // kdebug!("to_write");
    // let r = file.write(128, &buf);
    // kdebug!("write ok, r = {r:?}");
    // let r = file.read(128, &mut buf);
    // kdebug!("r = {r:?}, buf={buf:?}");
    // for x in buf.iter() {
    //     print!("{}", *x as char);
    // }
    // kdebug!("read ok");
    // kdebug!("file={file:?}, metadata = {:?}", file.metadata());

    // ======== 测试创建文件夹 ============

    // kdebug!(" to create dir 'test_create'.");
    // let r: Result<Arc<dyn IndexNode>, i32> = fat_root.create("test_create", FileType::Dir, 0o777);
    // kdebug!("test_create  r={r:?}");
    // let test_create_inode = r.unwrap();
    // fat_root.create("test1", FileType::Dir, 0o777);
    // let test2 = fat_root.create("test2", FileType::Dir, 0o777).unwrap();

    // let test_create_inode = fat_root.create("test3", FileType::Dir, 0o777).unwrap();
    // fat_root.create("test4", FileType::Dir, 0o777);
    // fat_root.create("test5", FileType::Dir, 0o777);
    // fat_root.create("test6", FileType::Dir, 0o777);
    kdebug!("fat_root.list={:?}", fat_root.list());
    // kdebug!("test_create_inode.list={:?}", test_create_inode.list());
    // kdebug!("test_create_inode.metadata()={:?}", test_create_inode.metadata());

    // let r = test_create_inode.create("test_dir", FileType::Dir, 0o777);
    // kdebug!("test_dir  r={r:?}");
    // let test_dir = r.unwrap();
    // kdebug!("test_dir.list = {:?}", test_dir.list());
    // let r = test_create_inode.create("test_file", FileType::File, 0o777);
    // kdebug!("create test_file  r={r:?}");
    // let r = test_create_inode.create("test_file2", FileType::File, 0o777);
    // kdebug!("create test_file2  r={r:?}");
    // let test_file = File::new(test_create_inode.find("test_file").unwrap(), O_RDWR);
    // kdebug!("test_file  r={test_file:?}");
    // let mut test_file2 = File::new(test_create_inode.find("test_file2").unwrap(), O_RDWR).unwrap();
    // kdebug!("test_file2  r={test_file2:?}");
    // let mut test_file = test_file.unwrap();
    // kdebug!("test_file metadata = {:?}", test_file.metadata());
    // let mut buf:Vec<u8> = Vec::new();
    // for i in 0..10{
    //     buf.append(&mut format!("{}\n", i).as_bytes().to_vec());
    // }
    // let r = test_file.write(buf.len(), &buf);
    // kdebug!("write file, r= {r:?}");
    // let r = test_file2.write(buf.len(), &buf);
    // kdebug!("write file, r= {r:?}");

    // buf.clear();
    // buf.resize(64, 0);
    // let r = test_file.read(64, &mut buf);
    // kdebug!("read test_file, r={r:?}");
    // for x in buf.iter(){
    //     print!("{}", *x as char);
    // }


    // 测试删除文件

    // let test3 = fat_root.find("test3").unwrap();
    // let r = test3.unlink("test_dir");
    // kdebug!("r = {r:?}");
    
    // let a = test3.find("test_dir").unwrap().unlink("a");
    // assert!(a.is_ok());
    // let r = test3.unlink("test_dir");
    // assert!(r.is_ok());
    
    

    kdebug!("test_done");
    compiler_fence(Ordering::SeqCst);
}

fn __test_rootfs() {
    kdebug!("root inode.list()={:?}", ROOT_INODE.list());
}

fn __as_any_ref<T: Any>(x: &T) -> &dyn core::any::Any {
    x
}

/// @brief procfs测试函数
pub fn _test_procfs(pid: i64) {
    // __test_procfs(pid);
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
    let proc_inode = _t
        .find("status")
        .expect(&format!("Cannot find /proc/{}/status", pid));
    let mut f = File::new(proc_inode, O_RDONLY).unwrap();
    kdebug!("file created!");
    kdebug!("proc.list()={:?}", _p.list().expect("list /proc failed."));
    let mut buf: Vec<u8> = Vec::new();
    buf.resize(f.metadata().unwrap().size as usize, 0);

    let size = f
        .read(f.metadata().unwrap().size as usize, buf.as_mut())
        .unwrap();
    kdebug!("size = {}, data={:?}", size, buf);
    let buf = String::from_utf8(buf).unwrap();
    kdebug!("data for /proc/{}/status: {}", pid, buf);
}
