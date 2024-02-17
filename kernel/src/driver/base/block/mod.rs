pub mod block_device;
pub mod cache;
pub mod disk_info;
#[derive(Debug)]
#[allow(dead_code)]
pub enum SeekFrom {
    SeekSet(i64),
    SeekCurrent(i64),
    SeekEnd(i64),
    Invalid,
}
