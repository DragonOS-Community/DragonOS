use core::ffi::{c_char, c_void};
use libc::{mount, MS_BIND};
use std::time;
fn main() {
    let clock = time::Instant::now();
    let source = b"\0".as_ptr() as *const c_char;
    let target = b"/mnt/tmp\0".as_ptr() as *const c_char;
    let fstype = b"ramfs\0".as_ptr() as *const c_char;
    let flags = MS_BIND;
    let data = std::ptr::null() as *const c_void;
    let result = unsafe { mount(source, target, fstype, flags, data) };

    if result == 0 {
        println!("Mount successful");
    } else {
        println!("Mount failed");
    }
    let dur = clock.elapsed();
    println!("mount costing time: {} ns", dur.as_nanos());
}
