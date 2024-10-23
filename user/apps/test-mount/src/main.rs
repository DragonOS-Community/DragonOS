use core::ffi::{c_char, c_void};
use errno::errno;
use libc::{mount, MS_BIND};
use std::fs;
use std::path::Path;
use std::time;

fn main() {
    let path = Path::new("mnt/tmp");
    let dir = fs::create_dir_all(path);
    if dir.is_err() {
        panic!("mkdir /mnt/tmp fail.");
    }
    let clock = time::Instant::now();
    let source = b"\0".as_ptr() as *const c_char;
    let target = b"/mnt/tmp\0".as_ptr() as *const c_char;
    let fstype = b"ramfs\0".as_ptr() as *const c_char;
    let flags = MS_BIND;
    let data = std::ptr::null() as *const c_void;
    let result = unsafe { mount(source, target, fstype, flags, data) };
    let path = Path::new("mnt/tmp/tmp");
    let dir = fs::create_dir_all(path);
    if dir.is_err() {
        panic!("mkdir /mnt/tmp/tmp fail.");
    }
    let target = b"/mnt/tmp/tmp\0".as_ptr() as *const c_char;
    let result = unsafe { mount(source, target, fstype, flags, data) };
    if result == 0 {
        println!("Mount successful");
    } else {
        let err = errno();
        println!("Mount failed with error code: {}", err.0);
    }
    let dur = clock.elapsed();
    println!("mount costing time: {} ns", dur.as_nanos());
}
