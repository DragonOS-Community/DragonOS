use libc::{
    chown, fchown, fchownat, getgrnam, getpwnam, gid_t, lchown, uid_t, AT_FDCWD,
    AT_SYMLINK_NOFOLLOW,
};
use std::ffi::CString;
use std::fs::File;
use std::io::{Error, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;

fn print_file_owner_group(filename: &str) -> Result<(), Error> {
    let metadata = std::fs::metadata(filename)?;
    let uid = metadata.uid();
    let gid = metadata.gid();

    // let pw = unsafe { getpwnam(CString::new(uid.to_string())?.as_ptr()) };
    // let gr = unsafe { getgrnam(CString::new(gid.to_string())?.as_ptr()) };

    // if gr.is_null() {
    //     eprintln!("Invalid GID");
    //     return Err(Error::last_os_error());
    // }

    // if pw.is_null() {
    //     eprintln!("Invalid UID");
    //     return Err(Error::last_os_error());
    // }

    println!("File: {}", filename);
    println!("Owner UID: {}", uid);
    println!("Group GID: {}", gid);
    Ok(())
}
// fn test_fchownat(filename: &str, new_uid: uid_t, new_gid: gid_t, flags: i32) -> Result<(), Error> {
//     println!("Testing fchownat on file: {}", filename);
//     let c_filename = CString::new(filename)?;
//     let result = unsafe { fchownat(AT_FDCWD, c_filename.as_ptr(), new_uid, new_gid, flags) };

//     if result == -1 {
//         return Err(Error::last_os_error());
//     }

//     print_file_owner_group(filename)
// }

fn test_chown(filename: &str, new_uid: uid_t, new_gid: gid_t) -> Result<(), Error> {
    println!("Testing chown on file: {}", filename);
    let c_filename = CString::new(filename)?;
    let result = unsafe { chown(c_filename.as_ptr(), new_uid, new_gid) };

    if result == -1 {
        return Err(Error::last_os_error());
    }

    print_file_owner_group(filename)
}

fn test_fchown(fd: i32, new_uid: uid_t, new_gid: gid_t) -> Result<(), Error> {
    println!("Testing fchown on file descriptor");
    let result = unsafe { fchown(fd, new_uid, new_gid) };

    if result == -1 {
        return Err(Error::last_os_error());
    }

    // print_file_owner_group(filename);

    Ok(())
}

// fn test_lchown(symlink_name: &str, new_uid: uid_t, new_gid: gid_t) -> Result<(), Error> {
//     println!("Testing lchown on symbolic link: {}", symlink_name);
//     let c_symlink = CString::new(symlink_name)?;
//     let result = unsafe { lchown(c_symlink.as_ptr(), new_uid, new_gid) };

//     if result == -1 {
//         return Err(Error::last_os_error());
//     }

//     print_file_owner_group(symlink_name)
// }

fn main() -> Result<(), Error> {
    let filename = "testfile.txt";
    let symlink_name = "testsymlink";
    let new_owner = "nobody"; // 替换为你测试系统中的有效用户名
    let new_group = "nogroup"; // 替换为你测试系统中的有效组名

    // 获取新的 UID 和 GID
    let pw = unsafe { getpwnam(CString::new(new_owner)?.as_ptr()) };
    let gr = unsafe { getgrnam(CString::new(new_group)?.as_ptr()) };

    if pw.is_null() || gr.is_null() {
        eprintln!("Invalid user or group name");
        return Err(Error::last_os_error());
    }

    let new_uid = unsafe { (*pw).pw_uid };
    let new_gid = unsafe { (*gr).gr_gid };

    // 创建测试文件
    let mut file = File::create(filename)?;
    writeln!(file, "This is a test file for chown system call")?;

    // 创建符号链接
    // std::os::unix::fs::symlink(filename, symlink_name)?;

    // 打开文件以测试 fchown
    let fd = file.as_raw_fd();

    // 测试 chown
    test_chown(filename, new_uid, new_gid)?;

    // 测试 fchown
    test_fchown(fd, new_uid, new_gid)?;

    // // 测试 lchown
    // test_lchown(symlink_name, new_uid, new_gid)?;

    // // 测试 fchownat，带 AT_SYMLINK_NOFOLLOW 标志（不会跟随符号链接）
    // test_fchownat(symlink_name, new_uid, new_gid, AT_SYMLINK_NOFOLLOW)?;

    // 清理测试文件
    std::fs::remove_file(filename)?;

    Ok(())
}
