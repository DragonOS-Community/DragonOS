//
// Created by longjin on 2022/1/21.
//
#pragma once

#define PAD_ZERO 1  // 0填充
#define LEFT 2      // 靠左对齐
#define RIGHT 4     // 靠右对齐
#define PLUS 8      // 在正数前面显示加号
#define SPACE 16
#define SPECIAL 32  // 在八进制数前面显示 '0o'，在十六进制数前面显示 '0x' 或 '0X'
#define SMALL 64    // 十进制以上数字显示小写字母
#define SIGN 128    // 显示符号位


#define is_digit(c) ((c) >= '0' && (c) <= '9') // 用来判断是否是数字的宏

#include "font.h"
#include "glib.h"
#include <stdarg.h>

struct screen_info
{
    int width, height; //屏幕大小

    int x, y; //光标位置

    int char_size_x, char_size_y;

    unsigned int *FB_address; //帧缓冲区首地址
    unsigned long FB_length;  // 帧缓冲区长度
} pos;

extern unsigned char font_ascii[256][16]; //导出ascii字体的bitmap（8*16大小）

char buf[4096]; //vsprintf()的缓冲区


/**
     * 将字符串按照fmt和args中的内容进行格式化，然后保存到buf中
     * @param buf 结果缓冲区
     * @param fmt 格式化字符串
     * @param args 内容
     * @return 最终字符串的长度
     */
static int vsprintf(char *buf, const char *fmt, va_list args);


/**
 * @brief 将数字按照指定的要求转换成对应的字符串（2~36进制）
 * 
 * @param str 要返回的字符串
 * @param num 要打印的数值
 * @param base 基数
 * @param field_width 区域宽度 
 * @param precision 精度
 * @param flags 标志位
 */
static void write_num(char* str, long long num, int base, int field_width, int precision, int flags);