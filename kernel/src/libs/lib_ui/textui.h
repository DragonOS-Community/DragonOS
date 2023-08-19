#pragma once
#include <common/glib.h>

/**
 * @brief 在默认窗口上输出一个字符
 *
 * @param character 字符
 * @param FRcolor 前景色（RGB）
 * @param BKcolor 背景色（RGB）
 * @return int
 */
extern int rs_textui_putchar(uint16_t character, uint32_t FRcolor, uint32_t BKcolor);

/**
 * @brief 初始化text ui框架
 *
 * @return int
 */
extern int textui_init();