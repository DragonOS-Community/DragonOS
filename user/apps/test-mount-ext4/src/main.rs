use core::ffi::{c_char, c_void};
use errno::errno;
use libc::{mount, umount, MS_BIND};
use std::fs;
use std::path::Path;
use std::time;

fn main() {
    let ext4_path = Path::new("mnt/ext4");
    let dir = fs::create_dir_all(ext4_path);
    if dir.is_err() {
        panic!("mkdir /mnt/ext4 fail.");
    }

    let clock = time::Instant::now();
    let source = b"/dev/vdb1\0".as_ptr() as *const c_char;
    let target = b"/mnt/ext4\0".as_ptr() as *const c_char;
    let fstype = b"ext4\0".as_ptr() as *const c_char;
    let flags = MS_BIND;
    let data = std::ptr::null() as *const c_void;
    let result = unsafe { mount(source, target, fstype, flags, data) };

    let path = Path::new("mnt/ext4/tmp");
    let dir = fs::create_dir_all(path);
    if dir.is_err() {
        panic!("mkdir /mnt/ext4/tmp fail.");
    }
    let _ = fs::remove_dir_all(path);

    if result == 0 {
        println!("Mount successful");
    } else {
        let err = errno();
        println!("Mount failed with error code: {}", err.0);
    }
    let dur = clock.elapsed();
    println!("mount costing time: {} ns", dur.as_nanos());

    let result = unsafe { umount(target) };
    if result != 0 {
        let err = errno();
        println!("Mount failed with error code: {}", err.0);
    }
    assert_eq!(result, 0, "Umount ext4 failed");
    println!("Umount successful");

    let _ = fs::remove_dir_all(ext4_path);

    println!("All tests passed!");
}
