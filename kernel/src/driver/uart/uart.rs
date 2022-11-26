use uart_core;
use crate::driver::core;

impl file_operations for uart_driver {
    /**
    * @brief initialze uart
    *
    * @param port com口的端口号
    * @param bits_rate 通信的比特率
    */
    fn init(&mut self, baud_rate: u32) {

    }

    fn send(&self) {

    }

    fn read(&self) {

    }
}