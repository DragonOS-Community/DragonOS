//
// Created by longjin on 2022/1/21.
//
#pragma once

#define PAD_ZERO 1 // 0填充
#define LEFT 2     // 靠左对齐
#define RIGHT 4    //靠右对齐
#define PLUS 8     // 在正数前面显示加号
#define SPACE 16
#define SPECIAL 32 //在八进制数前面显示 '0o'，在十六进制数前面显示 '0x' 或 '0X'


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
