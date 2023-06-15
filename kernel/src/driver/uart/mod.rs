use super::base::device::{Device, DeviceType};

pub mod uart;

pub trait UartOperations {
    fn open(baud_rate: u32) -> Result<i32, &'static str>;

    fn close();

    fn start();

    fn send(s: &str);

    fn stop();
}
