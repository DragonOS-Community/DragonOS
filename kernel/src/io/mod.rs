pub mod block;
pub mod device;
pub mod disk_info;

#[derive(Debug)]
pub enum SeekFrom {
    SeekSet(i64),
    SeekCurrent(i64),
    SeekEnd(i64),
    Invalid,
}
