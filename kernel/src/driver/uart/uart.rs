use crate::kdebug;
use crate::include::bindings::bindings::io_in8;

const COM1: u32 = 0x3f8;
// pub const COM2: u32 = 0x2f8;
// pub const COM3: u32 = 0x3e8;
// pub const COM4: u32 = 0x2e8;
// pub const COM5: u32 = 0x5f8;
// pub const COM6: u32 = 0x4f8;
// pub const COM7: u32 = 0x5e8;
// pub const COM8: u32 = 0x4E8;
pub struct UartRegister {
    pub reg_data: u8,
    pub reg_interrupt_enable: u8,
    pub reg_ii_fifo: u8,    // 	Interrupt Identification and FIFO control registers
    pub reg_line_config: u8,
    pub reg_modem_config: u8,
    pub reg_line_status: u8,
    pub reg_modem_statue: u8,
    pub reg_scartch: u8,
}

pub struct UartDriver {
    pub name: &'static str,
    pub addr: u32,
    pub register: UartRegister,
    pub baud_rate: u32,
}

impl UartDriver {
    fn init(&mut self, port: u32, baud_rate: u32) -> Result<(), &'static str> {
        self.addr = port;
        self.baud_rate = baud_rate;
        Ok(())
    }
}

#[no_mangle]
pub extern "C" fn uart_init(port: u32, baud_rate: u32) {
    kdebug!("uart_init");
    let register = UartRegister {
        reg_data: 0,
        reg_interrupt_enable: 1,
        reg_ii_fifo: 2,    // 	Interrupt Identification and FIFO control registers
        reg_line_config: 3,
        reg_modem_config: 4,
        reg_line_status: 5,
        reg_modem_statue: 6,
        reg_scartch: 7
    };
    let uart_driver = UartDriver {
        name: "uart_driver",
        addr: port,
        register: register,
        baud_rate: baud_rate,
    };
}

#[no_mangle]
pub extern "C" fn serial_received(offset: u16) -> bool {
    unsafe {
        if (io_in8(offset + 5) & 1) != 0 {
            true
        } else {
            false
        }
    }
}

#[no_mangle]
pub extern "C" fn is_transmit_empty(offset: u16) -> bool {
    unsafe {
        if (io_in8(offset + 5) & 0x20) != 0 {
            true
        } else {
            false
        }
    }
}