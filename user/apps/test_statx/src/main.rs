use libc::syscall;
use libc::AT_FDCWD;
use std::ffi::CString;

#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct Statx {
    pub stx_mask: u32,
    pub stx_blksize: u32,
    pub stx_attributes: u64,
    pub stx_nlink: u32,
    pub stx_uid: u32,
    pub stx_gid: u32,
    pub stx_mode: u16,
    __statx_pad1: [u16; 1],
    pub stx_ino: u64,
    pub stx_size: u64,
    pub stx_blocks: u64,
    pub stx_attributes_mask: u64,
    pub stx_atime: StatxTimestamp,
    pub stx_btime: StatxTimestamp,
    pub stx_ctime: StatxTimestamp,
    pub stx_mtime: StatxTimestamp,
    pub stx_rdev_major: u32,
    pub stx_rdev_minor: u32,
    pub stx_dev_major: u32,
    pub stx_dev_minor: u32,
    pub stx_mnt_id: u64,
    pub stx_dio_mem_align: u32,
    pub stx_dio_offset_align: u32,
    __statx_pad3: [u64; 12],
}

#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct StatxTimestamp {
    pub tv_sec: i64,
    pub tv_nsec: u32,
    pub __statx_timestamp_pad1: [i32; 1],
}

fn main() {
    let path = CString::new("/bin/about.elf").expect("Failed to create CString");
    let mut statxbuf: Statx = unsafe { std::mem::zeroed() };
    let x = sc::nr::STATX as i64;

    let result = unsafe {
        syscall(
            x,
            AT_FDCWD,
            path.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
            0x7ff,
            &mut statxbuf,
        )
    };

    if result != -1 {
        println!("statx succeeded: {:?}", statxbuf);
    } else {
        eprintln!("statx failed");
    }
}
