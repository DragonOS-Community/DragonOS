pub mod uart_core {
    pub enum uart_port_io_addr {
        COM1(u32),
        COM2(u32),
        COM3(u32),
        COM4(u32),
        COM5(u32),
        COM6(u32),
        COM7(u32),
        COM8(u32),
    }

    pub struct uart_register {
        reg_data: u8,
        reg_interrupt_enable: u8,
        reg_ii_fifo: u8,    // 	Interrupt Identification and FIFO control registers
        reg_line_config: u8,
        reg_modem_config: u8,
        reg_line_status: u8,
        reg_modem_statue: u8,
        reg_scartch: u8,
    }
    
    pub struct uart_driver {
        name: &str,
        addr: uart_port_io_addr,
        register: uart_register,
    }
}

const COM1: uart_port_io_addr = 0x3f8;
const COM2: uart_port_io_addr = 0x2f8;
const COM3: uart_port_io_addr = 0x3e8;
const COM4: uart_port_io_addr = 0x2e8;
const COM5: uart_port_io_addr = 0x5f8;
const COM6: uart_port_io_addr = 0x4f8;
const COM7: uart_port_io_addr = 0x5e8;
const COM8: uart_port_io_addr = 0x4E8;