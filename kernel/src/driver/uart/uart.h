#include <common/glib.h>

//driver/uart/uart.rs --rust function
extern const uint16_t COM1 = 0x3f8;
extern int c_uart_init(uint16_t port, uint32_t baud_rate);
extern void c_uart_send(uint16_t port, char c);
extern void c_uart_send_str(uint16_t port, const char *str);