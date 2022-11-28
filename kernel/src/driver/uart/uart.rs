use crate::kdebug;
use crate::include::bindings::bindings::{io_in8, io_out8};
use core::str;

const UART_SUCCESS: i32 = 0;
const E_UART_BITS_RATE_ERROR: i32 = 1;
const E_UART_SERIAL_FAULT: i32 = 2;
const UART_MAX_BITS_RATE: u32 = 115200;
pub const COM1: u16 = 0x3f8;
// pub const COM2: u16 = 0x2f8;
// pub const COM3: u16 = 0x3e8;
// pub const COM4: u16 = 0x2e8;
// pub const COM5: u16 = 0x5f8;
// pub const COM6: u16 = 0x4f8;
// pub const COM7: u16 = 0x5e8;
// pub const COM8: u16 = 0x4E8;

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
    pub fn serial_received(offset: u16) -> bool {
        unsafe {
            if (io_in8(offset + 5) & 1) != 0 {
                true
            } else {
                false
            }
        }
    }
    
    pub fn is_transmit_empty(offset: u16) -> bool {
        unsafe {
            if (io_in8(offset + 5) & 0x20) != 0 {
                true
            } else {
                false
            }
        }
    }
}

/**
 * @brief 发送数据
 *
 * @param port 端口号
 * @param c 要发送的数据
 */
pub extern "C" fn uart_send(port: u16, c: u8) {
    while UartDriver::is_transmit_empty(port) == false {} //TODO:pause
    unsafe { io_out8(port, c); }
}

/**
 * @brief 从uart接收数据
 *
 * @param port 端口号
 * @return u8 接收到的数据
 */
#[no_mangle]
pub extern "C" fn uart_read(port: u16) -> u8 {
    while UartDriver::serial_received(port) == false {} //TODO:pause
    unsafe { return io_in8(port); }
}

/**
 * @brief 通过串口发送整个字符串
 *
 * @param port 串口端口
 * @param str 字符串
 */
#[no_mangle]
pub extern "C" fn uart_send_str(port: u16, str: *const u8)
{
    let message: &'static str = "uart send str";
    let bytes = message.as_bytes();
    for c in bytes {
        uart_send(port, *c);
    }
    unsafe { uart_send(port, *str) };
}

#[no_mangle]
pub extern "C" fn uart_init(port: u16, baud_rate: u32) -> i32 {
    let message: &'static str = "uart init";
    kdebug!("{}", message);
    // 错误的比特率
    if baud_rate > UART_MAX_BITS_RATE || UART_MAX_BITS_RATE % baud_rate != 0 {
        return -E_UART_BITS_RATE_ERROR;
    }

    unsafe {
        io_out8(port + 1, 0x00); // Disable all interrupts
        io_out8(port + 3, 0x80); // Enable DLAB (set baud rate divisor)
    
        let divisor = UART_MAX_BITS_RATE / baud_rate;
        
        io_out8(port + 0, (divisor & 0xff) as u8);        // Set divisor  (lo byte)
        io_out8(port + 1, ((divisor >> 8) & 0xff) as u8); //                  (hi byte)
        io_out8(port + 3, 0x03);                  // 8 bits, no parity, one stop bit
        io_out8(port + 2, 0xC7);                  // Enable FIFO, clear them, with 14-byte threshold
        io_out8(port + 4, 0x08); // IRQs enabled, RTS/DSR clear (现代计算机上一般都不需要hardware flow control，因此不需要置位RTS/DSR)
        io_out8(port + 4, 0x1E); // Set in loopback mode, test the serial chip
        io_out8(port + 0, 0xAE); // Test serial chip (send byte 0xAE and check if serial returns same byte)
    
        // Check if serial is faulty (i.e: not same byte as sent)
        if io_in8(port + 0) != 0xAE {
            return -E_UART_SERIAL_FAULT;
        }
    
        // If serial is not faulty set it in normal operation mode
        // (not-loopback with IRQs enabled and OUT#1 and OUT#2 bits enabled)
        io_out8(port + 4, 0x08);
        let bytes = message.as_bytes();
        for c in bytes {
            uart_send(port, *c);
        }
    }
    return UART_SUCCESS;
    /*
            Notice that the initialization code above writes to [PORT + 1]
        twice with different values. This is once to write to the Divisor
        register along with [PORT + 0] and once to write to the Interrupt
        register as detailed in the previous section.
            The second write to the Line Control register [PORT + 3]
        clears the DLAB again as well as setting various other bits.
    */
}
