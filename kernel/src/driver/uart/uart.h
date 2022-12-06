#include <common/glib.h>

//driver/uart/uart.rs --rust function
enum uart_port_io_addr
{
    COM1 = 0x3f8,
    COM2 = 0x2f8,
    COM3 = 0x3e8,
    COM4 = 0x2e8,
    COM5 = 0x5f8,
    COM6 = 0x4f8,
    COM7 = 0x5e8,
    COM8 = 0x4E8,
};
extern int c_uart_init(uint16_t port, uint32_t baud_rate);
extern void c_uart_send(uint16_t port, char c);
extern void c_uart_send_str(uint16_t port, const char *str);