use core::ffi::{c_char, c_void};
use libc::{
    chown, fchown, fchownat, getgrnam, getpwnam, gid_t, lchown, mount, uid_t, umount, AT_FDCWD,
    AT_SYMLINK_NOFOLLOW,
};
use nix::errno::Errno;
use std::{
    ffi::CString,
    fs::{self, metadata, File},
    io::{self, Error, Write},
    os::unix::{
        fs::{MetadataExt, PermissionsExt},
        io::AsRawFd,
    },
    path::Path,
};

fn print_file_owner_group(filename: &str) -> Result<(), Error> {
    let metadata = std::fs::metadata(filename)?;
    let uid = metadata.uid();
    let gid = metadata.gid();

    // 确保 UID 和 GID 打印正确
    assert!(uid > 0, "UID should be greater than 0");
    assert!(gid > 0, "GID should be greater than 0");

    Ok(())
}

fn test_fchownat(filename: &str, new_uid: uid_t, new_gid: gid_t, flags: i32) -> Result<(), Error> {
    let c_filename = CString::new(filename)?;
    let result = unsafe { fchownat(AT_FDCWD, c_filename.as_ptr(), new_uid, new_gid, flags) };

    // 确保 fchownat 成功
    assert!(result != -1, "fchownat failed");

    print_file_owner_group(filename)?;
    Ok(())
}

fn test_chown(filename: &str, new_uid: uid_t, new_gid: gid_t) -> Result<(), Error> {
    let c_filename = CString::new(filename)?;
    let result = unsafe { chown(c_filename.as_ptr(), new_uid, new_gid) };

    // 确保 chown 成功
    assert!(result != -1, "chown failed");

    print_file_owner_group(filename)?;
    Ok(())
}

fn test_fchown(fd: i32, new_uid: uid_t, new_gid: gid_t) -> Result<(), Error> {
    let result = unsafe { fchown(fd, new_uid, new_gid) };

    // 确保 fchown 成功
    assert!(result != -1, "fchown failed");

    Ok(())
}

fn test_lchown(symlink_name: &str, new_uid: uid_t, new_gid: gid_t) -> Result<(), Error> {
    let c_symlink = CString::new(symlink_name)?;
    let result = unsafe { lchown(c_symlink.as_ptr(), new_uid, new_gid) };

    // 确保 lchown 成功
    assert!(result != -1, "lchown failed");

    print_file_owner_group(symlink_name)?;
    Ok(())
}

fn main() -> Result<(), Error> {
    mount_test_ramfs();

    let filename = "/mnt/myramfs/testfile.txt";
    let symlink_name = "/mnt/myramfs/testsymlink";
    let new_owner = "nobody"; // 替换为你测试系统中的有效用户名
    let new_group = "nogroup"; // 替换为你测试系统中的有效组名

    // 获取新的 UID 和 GID
    let pw = unsafe { getpwnam(CString::new(new_owner)?.as_ptr()) };
    let gr = unsafe { getgrnam(CString::new(new_group)?.as_ptr()) };

    assert!(!pw.is_null(), "Invalid user name");
    assert!(!gr.is_null(), "Invalid group name");

    let new_uid = unsafe { (*pw).pw_uid };
    let new_gid = unsafe { (*gr).gr_gid };

    // 创建测试文件
    let mut file = File::create(filename)?;
    println!("Created test file: {}", filename);
    writeln!(file, "This is a test file for chown system call")?;

    // 创建符号链接
    std::os::unix::fs::symlink(filename, symlink_name)?;
    println!("Created symlink: {}", symlink_name);

    // 打开文件以测试 fchown
    let fd = file.as_raw_fd();

    // 测试 chown
    test_chown(filename, new_uid, new_gid)?;

    // 测试 fchown
    test_fchown(fd, new_uid, new_gid)?;

    // 测试 lchown
    test_lchown(symlink_name, new_uid, new_gid)?;

    // 测试 fchownat，带 AT_SYMLINK_NOFOLLOW 标志（不会跟随符号链接）
    test_fchownat(symlink_name, new_uid, new_gid, AT_SYMLINK_NOFOLLOW)?;

    // 清理测试文件
    std::fs::remove_file(filename)?;

    umount_test_ramfs();

    println!("All tests passed!");

    Ok(())
}

fn mount_test_ramfs() {
    let path = Path::new("mnt/myramfs");
    let dir = fs::create_dir_all(path);
    assert!(dir.is_ok(), "mkdir /mnt/myramfs failed");

    let source = b"\0".as_ptr() as *const c_char;
    let target = b"/mnt/myramfs\0".as_ptr() as *const c_char;
    let fstype = b"ramfs\0".as_ptr() as *const c_char;
    // let flags = MS_BIND;
    let flags = 0;
    let data = std::ptr::null() as *const c_void;
    let result = unsafe { mount(source, target, fstype, flags, data) };

    assert_eq!(
        result,
        0,
        "Mount myramfs failed, errno: {}",
        Errno::last().desc()
    );
    println!("Mount myramfs for test success!");
}

fn umount_test_ramfs() {
    let path = b"/mnt/myramfs\0".as_ptr() as *const c_char;
    let result = unsafe { umount(path) };
    if result != 0 {
        let err = Errno::last();
        println!("Errno: {}", err);
        println!("Infomation: {}", err.desc());
    } else {
        // 删除mnt/myramfs
        let path = Path::new("mnt/myramfs");
        let _ = fs::remove_dir(path);
    }
    assert_eq!(result, 0, "Umount myramfs failed");
    println!("Umount myramfs for test success!");
}
