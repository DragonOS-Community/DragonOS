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
//#include "linkage.h"
#include <stdarg.h>

struct printk_screen_info
{
    int width, height; //屏幕大小

    int max_x, max_y; // 最大x、y字符数

    int x, y; //光标位置

    int char_size_x, char_size_y;

    uint *FB_address; //帧缓冲区首地址

    unsigned long FB_length; // 帧缓冲区长度（乘以4才是字节数）
};

extern unsigned char font_ascii[256][16]; //导出ascii字体的bitmap（8*16大小） ps:位于font.h中



/**
 * @brief 初始化printk的屏幕信息
 *
 * @param char_size_x 字符的列坐标
 * @param char_size_y 字符的行坐标
 */
int printk_init(const int char_size_x, const int char_size_y);

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
 * @brief 格式化打印字符串
 *
 * @param FRcolor 前景色
 * @param BKcolor 背景色
 * @param ... 格式化字符串
 */

#define printk(...) printk_color(WHITE, BLACK, __VA_ARGS__)

int printk_color(unsigned int FRcolor, unsigned int BKcolor, const char *fmt, ...);




/**
 * @brief 获取VBE帧缓冲区长度

 */
ul get_VBE_FB_length();

/**
 * @brief 设置pos变量中的VBE帧缓存区的线性地址
 * @param virt_addr VBE帧缓存区线性地址
 */
void set_pos_VBE_FB_addr(uint* virt_addr);



/**
 * @brief 使能滚动动画
 * 
 */
void printk_enable_animation();
/**
 * @brief 禁用滚动动画
 * 
 */
void printk_disable_animation();

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