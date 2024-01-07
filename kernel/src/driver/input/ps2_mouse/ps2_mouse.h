#pragma once

#include <common/glib.h>

#define PS2_MOUSE_INTR_VECTOR 0x2c // 鼠标的中断向量号

#define KEYBOARD_COMMAND_SEND_TO_PS2_MOUSE 0xd4 // 键盘控制器向鼠标设备发送数据的命令

#define PS2_MOUSE_GET_ID 0xf2                    // 获取鼠标的ID
#define PS2_MOUSE_SET_SAMPLING_RATE 0xf3         // 设置鼠标的采样率
#define PS2_MOUSE_ENABLE 0xf4                    // 允许鼠标设备发送数据包
#define PS2_MOUSE_DISABLE 0xf5                   // 禁止鼠标设备发送数据包
#define PS2_MOUSE_SET_DEFAULT_SAMPLING_RATE 0xf6 // 设置使用默认采样率100hz，分辨率4px/mm
#define PS2_MOUSE_RESEND_LAST_PACKET 0xfe        // 重新发送上一条数据包
#define PS2_MOUSE_RESET 0xff                     // 重启鼠标

#define KEYBOARD_COMMAND_ENABLE_PS2_MOUSE_PORT 0xa8 // 通过键盘控制器开启鼠标端口的命令

#define ps2_mouse_buffer_size 360

#define PORT_KEYBOARD_DATA 0x60
#define PORT_KEYBOARD_STATUS 0x64
#define PORT_KEYBOARD_CONTROL 0x64

#define KEYBOARD_COMMAND_WRITE 0x60 // 向键盘发送配置命令
#define KEYBOARD_COMMAND_READ 0x20  // 读取键盘的配置值
#define KEYBOARD_PARAM_INIT 0x47    // 初始化键盘控制器的配置值

// ========= 检测键盘控制器输入/输出缓冲区是否已满
#define KEYBOARD_FLAG_OUTBUF_FULL 0x01 // 键盘的输出缓冲区已满标志位
#define KEYBOARD_FLAG_INBUF_FULL 0x02  // 键盘的输入缓冲区已满标志位

// 等待向键盘控制器写入信息完成
#define wait_keyboard_write() while (io_in8(PORT_KEYBOARD_STATUS) & KEYBOARD_FLAG_INBUF_FULL)
// 等待从键盘控制器读取信息完成
#define wait_keyboard_read() while (io_in8(PORT_KEYBOARD_STATUS) & KEYBOARD_FLAG_OUTBUF_FULL)

#define SUCCESS 0
#define EINVALID_ARGUMENT -1
#define EFAIL -2

/**
 * @brief 初始化鼠标驱动程序
 *
 */
void ps2_mouse_init();

int ps2_mouse_set_sample_rate(unsigned int hz);
