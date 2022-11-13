#include "uart.h"
#include <common/kprint.h>

#define UART_MAX_BITS_RATE 115200

/**
 * @brief 当前是否有数据到达
 *
 */
#define serial_received(p) ((io_in8(p + 5) & 1))

/**
 * @brief 当前是否有数据正等待发送
 *
 */
#define is_transmit_empty(p) ((io_in8(p + 5) & 0x20))

/**
 * @brief 初始化com口
 *
 * @param port com口的端口号
 * @param bits_rate 通信的比特率
 */
int uart_init(uint32_t port, uint32_t bits_rate)
{
    // 错误的比特率
    if (bits_rate > UART_MAX_BITS_RATE || UART_MAX_BITS_RATE % bits_rate != 0)
        return E_UART_BITS_RATE_ERROR;

    io_out8(port + 1, 0x00); // Disable all interrupts
    io_out8(port + 3, 0x80); // Enable DLAB (set baud rate divisor)

    uint16_t divisor = UART_MAX_BITS_RATE / bits_rate;
    
    io_out8(port + 0, divisor & 0xff);        // Set divisor  (lo byte)
    io_out8(port + 1, (divisor >> 8) & 0xff); //                  (hi byte)
    io_out8(port + 3, 0x03);                  // 8 bits, no parity, one stop bit
    io_out8(port + 2, 0xC7);                  // Enable FIFO, clear them, with 14-byte threshold
    io_out8(port + 4, 0x08); // IRQs enabled, RTS/DSR clear (现代计算机上一般都不需要hardware flow control，因此不需要置位RTS/DSR)
    io_out8(port + 4, 0x1E); // Set in loopback mode, test the serial chip
    io_out8(port + 0, 0xAE); // Test serial chip (send byte 0xAE and check if serial returns same byte)

    // Check if serial is faulty (i.e: not same byte as sent)
    if (io_in8(port + 0) != 0xAE)
    {
        return E_UART_SERIAL_FAULT;
    }

    // If serial is not faulty set it in normal operation mode
    // (not-loopback with IRQs enabled and OUT#1 and OUT#2 bits enabled)
    io_out8(port + 4, 0x08);

    char init_text2[] = "uart initialized.\n";
    for (int i = 0; i < sizeof(init_text2) - 1; ++i)
        uart_send(COM1, init_text2[i]);
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

/**
 * @brief 发送数据
 *
 * @param port 端口号
 * @param c 要发送的数据
 */
void uart_send(uint32_t port, char c)
{
    while (is_transmit_empty(port) == 0)
        pause();
    io_out8(port, c);
}

/**
 * @brief 从uart接收数据
 *
 * @param port 端口号
 * @return uchar 接收到的数据
 */
uchar uart_read(uint32_t port)
{
    while (serial_received(port) == 0)
        pause();

    return io_in8(port);
}

/**
 * @brief 通过串口发送整个字符串
 *
 * @param port 串口端口
 * @param str 字符串
 */
void uart_send_str(uint32_t port, const char *str)
{
    if ((unlikely(str == NULL)))
        return;
    while (1)
    {
        if (unlikely(*str == '\0'))
            return;
        uart_send(port, *str);
        ++str;
    }
}