use core::ffi::{c_char, c_void};
use libc::{mount, MS_BIND};

fn main() {
    let source = b"/tmp\0".as_ptr() as *const c_char;
    let target = b"/mnt\0".as_ptr() as *const c_char;
    let fstype = b"ramfs".as_ptr() as *const c_char;
    let flags = MS_BIND;
    let data = std::ptr::null() as *const c_void;

    let result = unsafe { mount(source, target, fstype, flags, data) };

    if result == 0 {
        println!("Mount successful");
    } else {
        println!("Mount failed");
    }
}
