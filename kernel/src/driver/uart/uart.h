/**
 * @file uart.h
 * @author longjin (longjin@RinGoTek.cn)
 * @brief uart驱动程序 RS-232驱动
 * @version 0.1
 * @date 2022-04-15
 * 
 * @copyright Copyright (c) 2022
 * 
 */
#pragma once

#include <common/glib.h>

#define UART_SUCCESS 0
#define E_UART_BITS_RATE_ERROR 1
#define E_UART_SERIAL_FAULT 2
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

enum uart_register_offset
{
    REG_DATA = 0,
    REG_INTERRUPT_ENABLE = 1,
    REG_II_FIFO = 2,    // 	Interrupt Identification and FIFO control registers
    REG_LINE_CONTROL = 3,
    REG_MODEM_CONTROL = 4,
    REG_LINE_STATUS = 5,
    REG_MODEM_STATUE = 6,
    REG_SCRATCH = 7
};

/**
 * @brief 初始化com口
 * 
 * @param port com口的端口号
 * @param bits_rate 通信的比特率
 */
int uart_init(uint32_t port, uint32_t bits_rate);

/**
 * @brief 发送数据
 * 
 * @param port 端口号
 * @param c 要发送的数据
 */
void uart_send(uint32_t port, char c);

/**
 * @brief 从uart接收数据
 * 
 * @param port 端口号
 * @return uchar 接收到的数据
 */
uchar uart_read(uint32_t port);

/**
 * @brief 通过串口发送整个字符串
 *
 * @param port 串口端口
 * @param str 字符串
 */
void uart_send_str(uint32_t port, const char *str);