#pragma once

#include "../../common/glib.h"

// 定义键盘循环队列缓冲区大小为100bytes
#define keyboard_buffer_size 100

/**
 * @brief 键盘循环队列缓冲区结构体
 *
 */
struct keyboard_input_buffer
{
    unsigned char *ptr_head;
    unsigned char *ptr_tail;
    int count;
    unsigned char buffer[keyboard_buffer_size];
};

#define PORT_KEYBOARD_DATA 0x60
#define PORT_KEYBOARD_STATUS 0x64
#define PORT_KEYBOARD_CONTROL 0x64

#define KEYBOARD_COMMAND_WRITE 0x60 // 向键盘发送配置命令
#define KEYBOARD_COMMAND_READ 0x20  // 读取键盘的配置值
#define KEYBOARD_PARAM_INIT 0x47    // 初始化键盘控制器的配置值

// ========= 检测键盘输入/输出缓冲区是否已满
#define KEYBOARD_FLAG_OUTBUF_FULL 0x01 // 键盘的输出缓冲区已满标志位
#define KEYBOARD_FLAG_INBUF_FULL 0x02  // 键盘的输入缓冲区已满标志位

// 等待向键盘控制器写入信息完成
#define wait_keyboard_write() while (io_in8(PORT_KEYBOARD_STATUS) & KEYBOARD_FLAG_INBUF_FULL)
// 等待从键盘控制器读取信息完成
#define wait_keyboard_read() while (io_in8(PORT_KEYBOARD_STATUS) & KEYBOARD_FLAG_OUTBUF_FULL)

/**
 * @brief 初始化键盘驱动程序的函数
 * 
 */
void keyboard_init();

/**
 * @brief 键盘驱动卸载函数
 * 
 */
void keyboard_exit();