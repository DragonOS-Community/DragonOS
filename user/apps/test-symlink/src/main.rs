extern crate libc;
use core::ffi::{c_char, c_void};
use libc::{mount, umount};
use nix::errno::Errno;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;

fn main() {
    mount_test_ramfs();

    let target = "/mnt/myramfs/target_file.txt";
    let symlink_path = "/mnt/myramfs/another/symlink_file.txt";
    let dir = "/mnt/myramfs/another";

    fs::write(target, "This is the content of the target file.")
        .expect("Failed to create target file");
    fs::create_dir(dir).expect("Failed to create target dir");

    assert!(Path::new(target).exists(), "Target file was not created");
    assert!(Path::new(dir).exists(), "Target dir was not created");

    symlink(target, symlink_path).expect("Failed to create symlink");

    assert!(Path::new(symlink_path).exists(), "Symlink was not created");

    let symlink_content = fs::read_link(symlink_path).expect("Failed to read symlink");
    assert_eq!(
        symlink_content.display().to_string(),
        target,
        "Symlink points to the wrong target"
    );

    fs::remove_file(symlink_path).expect("Failed to remove symlink");
    fs::remove_file(target).expect("Failed to remove target file");
    fs::remove_dir(dir).expect("Failed to remove test_dir");

    assert!(!Path::new(symlink_path).exists(), "Symlink was not deleted");
    assert!(!Path::new(target).exists(), "Target file was not deleted");
    assert!(!Path::new(dir).exists(), "Directory was not deleted");

    umount_test_ramfs();

    println!("All tests passed!");
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
    println!("Mount myramfs success!");
}

fn umount_test_ramfs() {
    let path = b"/mnt/myramfs\0".as_ptr() as *const c_char;
    let result = unsafe { umount(path) };
    if result != 0 {
        let err = Errno::last();
        println!("Errno: {}", err);
        println!("Infomation: {}", err.desc());
    }
    assert_eq!(result, 0, "Umount myramfs failed");
}
