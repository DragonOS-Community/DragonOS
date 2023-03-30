#pragma once

#include <common/glib.h>

#define PS2_KEYBOARD_INTR_VECTOR 0x21 // 键盘的中断向量号

// 定义键盘循环队列缓冲区大小为100bytes
#define ps2_keyboard_buffer_size 8

#define KEYBOARD_CMD_RESET_BUFFER 1

#define PORT_PS2_KEYBOARD_DATA 0x60
#define PORT_PS2_KEYBOARD_STATUS 0x64
#define PORT_PS2_KEYBOARD_CONTROL 0x64

#define PS2_KEYBOARD_COMMAND_WRITE 0x60 // 向键盘发送配置命令
#define PS2_KEYBOARD_COMMAND_READ 0x20  // 读取键盘的配置值
#define PS2_KEYBOARD_PARAM_INIT 0x47    // 初始化键盘控制器的配置值

// ========= 检测键盘控制器输入/输出缓冲区是否已满
#define PS2_KEYBOARD_FLAG_OUTBUF_FULL 0x01 // 键盘的输出缓冲区已满标志位
#define PS2_KEYBOARD_FLAG_INBUF_FULL 0x02  // 键盘的输入缓冲区已满标志位

// 等待向键盘控制器写入信息完成
// todo: bugfix:在不包含ps2键盘控制器的机器上，这里会卡死
#define wait_ps2_keyboard_write() while (io_in8(PORT_PS2_KEYBOARD_STATUS) & PS2_KEYBOARD_FLAG_INBUF_FULL)
// #define wait_ps2_keyboard_write() (1)
// 等待从键盘控制器读取信息完成
#define wait_ps2_keyboard_read() while (io_in8(PORT_PS2_KEYBOARD_STATUS) & PS2_KEYBOARD_FLAG_OUTBUF_FULL)
// #define wait_ps2_keyboard_read() (1)

extern struct vfs_file_operations_t ps2_keyboard_fops;

/**
 * @brief 初始化键盘驱动程序的函数
 *
 */
void ps2_keyboard_init();

/**
 * @brief 键盘驱动卸载函数
 *
 */
void ps2_keyboard_exit();
