//
// Created by longjin on 2022/1/21.
//
#pragma once
#pragma GCC push_options
#pragma GCC optimize("O0")
#define PAD_ZERO 1 // 0填充
#define LEFT 2     // 靠左对齐
#define RIGHT 4    // 靠右对齐
#define PLUS 8     // 在正数前面显示加号
#define SPACE 16
#define SPECIAL 32 // 在八进制数前面显示 '0o'，在十六进制数前面显示 '0x' 或 '0X'
#define SMALL 64   // 十进制以上数字显示小写字母
#define SIGN 128   // 显示符号位

#define is_digit(c) ((c) >= '0' && (c) <= '9') // 用来判断是否是数字的宏

// 字体颜色的宏定义
#define WHITE 0x00ffffff  //白
#define BLACK 0x00000000  //黑
#define RED 0x00ff0000    //红
#define ORANGE 0x00ff8000 //橙
#define YELLOW 0x00ffff00 //黄
#define GREEN 0x0000ff00  //绿
#define BLUE 0x000000ff   //蓝
#define INDIGO 0x0000ffff //靛
#define PURPLE 0x008000ff //紫

// 异常的宏定义
#define EPOS_OVERFLOW 1 // 坐标溢出
#define EFB_MISMATCH 2  // 帧缓冲区与指定的屏幕大小不匹配
#define EUNSUPPORTED 3  // 当前操作暂不被支持

#include "font.h"
#include "glib.h"
#include <libs/libUI/screen_manager.h>
#include <stdarg.h>

extern unsigned char font_ascii[256][16]; //导出ascii字体的bitmap（8*16大小） ps:位于font.h中


/**
 * @brief 将字符串按照fmt和args中的内容进行格式化，然后保存到buf中
 *
 * @param buf 结果缓冲区
 * @param fmt 格式化字符串
 * @param args 内容
 * @return 最终字符串的长度
 */
int vsprintf(char *buf, const char *fmt, va_list args);

/**
 * @brief 将字符串按照fmt和args中的内容进行格式化，截取字符串前buf_size-1，保存到buf中
 *
 * @param buf 结果缓冲区，大小为buf_size
 * @param fmt 格式化字符串
 * @param buf_size 缓冲区长度
 * @param args 内容
 * @return 最终字符串的长度
 */
int vsnprintf(char *buf, const char *fmt, int buf_size, va_list args);

/**
 * @brief 格式化打印字符串
 *
 * @param FRcolor 前景色
 * @param BKcolor 背景色
 * @param ... 格式化字符串
 */

#define printk(...) printk_color(WHITE, BLACK, __VA_ARGS__)

int printk_color(unsigned int FRcolor, unsigned int BKcolor, const char *fmt, ...);

/**
 * @brief 格式化字符串并输出到buf
 *
 * @param buf 输出缓冲区
 * @param fmt 格式
 * @param ... 参数
 * @return int 字符串长度
 */
int sprintk(char *buf, const char *fmt, ...);
#pragma GCC pop_options