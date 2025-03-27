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
#define WHITE 0x00ffffff  // 白
#define BLACK 0x00000000  // 黑
#define RED 0x00ff0000    // 红
#define ORANGE 0x00ff8000 // 橙
#define YELLOW 0x00ffff00 // 黄
#define GREEN 0x0000ff00  // 绿
#define BLUE 0x000000ff   // 蓝
#define INDIGO 0x0000ffff // 靛
#define PURPLE 0x008000ff // 紫

// 异常的宏定义
#define EPOS_OVERFLOW 1 // 坐标溢出
#define EFB_MISMATCH 2  // 帧缓冲区与指定的屏幕大小不匹配
#define EUNSUPPORTED 3  // 当前操作暂不被支持

#include "glib.h"
#include <stdarg.h>

#pragma GCC pop_options