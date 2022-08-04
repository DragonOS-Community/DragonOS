#pragma once
#include <common/glib.h>
#include <common/sys/types.h>
#include <common/spinlock.h>

// 文本窗口标志位
// 文本窗口是否为彩色
#define TEXTUI_WF_CHROMATIC (1 << 0)

// 窗口是否启用彩色字符
#define textui_is_chromatic(flag) ((flag)&TEXTUI_WF_CHROMATIC)

// 每个字符的宽度和高度（像素）
#define TEXTUI_CHAR_WIDTH 8
#define TEXTUI_CHAR_HEIGHT 16

/**
 * @brief 黑白字符对象
 *
 */
struct textui_char_normal_t
{
    char c;
};

/**
 * @brief 彩色字符对象
 *
 */
struct textui_char_chromatic_t
{
    uint16_t c; // 字符
    // 前景色
    uint8_t Fr; // 红
    uint8_t Fg; // 绿
    uint8_t Fb; // 蓝

    // 背景色
    uint8_t Br;
    uint8_t Bg;
    uint8_t Bb;
};

// 注意！！！ 请保持vline结构体的大小、成员变量命名相等！
/**
 * @brief 单色显示的虚拟行结构体
 *
 */
struct textui_vline_normal_t
{
    struct textui_char_normal_t *chars; // 字符对象数组
    uint16_t index;                     // 当前操作的位置
};

/**
 * @brief 彩色显示的虚拟行结构体
 *
 */
struct textui_vline_chromatic_t
{
    struct textui_char_chromatic_t *chars;
    uint16_t index; // 当前操作的位置
};

/**
 * @brief textu ui 框架的文本窗口结构体
 *
 */
struct textui_window_t
{
    struct List list;

    uint32_t id;          // 窗口id
    uint16_t vlines_num;  // 虚拟行总数
    uint16_t vlines_used; // 当前已经使用了的虚拟行总数

    // 指向虚拟行的数组的指针（二选一）
    union
    {
        struct textui_vline_normal_t *normal;
        struct textui_vline_chromatic_t *chromatic;
    } vlines;

    uint16_t top_vline;       // 位于最顶上的那一个虚拟行的行号
    uint16_t vline_operating; // 正在操作的vline
    uint16_t chars_per_line;  // 每行最大容纳的字符数
    uint8_t flags;            // 窗口flag
    spinlock_t lock;          // 窗口操作锁
};

struct textui_private_info_t
{
    uint16_t actual_line;                   // 真实行的数量
    struct textui_window_t *current_window; // 当前的主窗口
    struct textui_window_t *default_window; // 默认print到的窗口
};

/**
 * @brief 重新渲染整个虚拟行
 *
 * @param window 窗口结构体
 * @param vline_id 虚拟行号
 * @return int 错误码
 */
int textui_refresh_vline(struct textui_window_t *window, uint16_t vline_id);

int textui_refresh_vlines(struct textui_window_t *window, uint16_t start, uint16_t count);

/**
 * @brief 刷新某个虚拟行的连续n个字符对象
 *
 * @param window 窗口结构体
 * @param vline_id 虚拟行号
 * @param start 起始字符号
 * @param count 要刷新的字符数量
 * @return int 错误码
 */
int textui_refresh_characters(struct textui_window_t *window, uint16_t vline_id, uint16_t start, uint16_t count);

/**
 * @brief 在指定窗口上输出一个字符
 *
 * @param window 窗口
 * @param character 字符
 * @return int
 */
int textui_putchar_window(struct textui_window_t *window, uint16_t character);

/**
 * @brief 在默认窗口上输出一个字符
 *
 * @param character 字符
 * @return int
 */
int textui_putchar(uint16_t character);

/**
 * @brief 获取textui的帧缓冲区能容纳的内容的行数
 *
 * @return uint16_t
 */
uint16_t __textui_get_actual_lines();

/**
 * @brief 获取当前渲染的窗口的id
 *
 * @return uint16_t
 */
uint32_t __textui_get_current_window_id();

/**
 * @brief 初始化text ui框架
 *
 * @return int
 */
int textui_init();