use alloc::string::String;

struct Device {
    name: String,
}

struct Driver {
    name: String,
}

pub trait FileOperations {
    fn init(&self) -> i32;
    fn open();
    fn close();
    fn ioctl();
}