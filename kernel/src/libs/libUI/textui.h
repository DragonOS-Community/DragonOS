#pragma once
#include <common/glib.h>

/*
textui中的几个对象的关系：


                                          textui_vline_normal_t
                                          +--------------------------------+
                                          |                                |        textui_char_normal_t
 textui_window_t                          | chars: textui_char_normal_t *  |        +--------------------------+
+----------------------------+            |                                |        |                          |
|                            |     +------>                                +-------->  c: char                 |
|  list:List                 |     |      | index:  int16_t                |        +--------------------------+
|  vlines_num:int16_t        |     |      |                                |
|  vlines_used:int16_t       |     |      +--------------------------------+
|                            |     |
|  vlines                    +-----+                                                textui_char_chromatic_t
|                            |     |       textui_vline_chromatic_t                 +--------------------------+
|  top_vline:int16_t         |     |      +-------------------------------------+   |                          |
|  vline_operating:int16_t   |     |      |                                     |   |   c: uint16_t            |
|  chars_per_line:int16_t    |     |      |  chars: textui_char_chromatic_t *   |   |                          |
|  flags:uint8_t             |     |      |                                     |   |   FRcolor:24             |
|  lock:spinlock_t           |     +------>                                     +--->                          |
|                            |            |  index:  int16_t                    |   |   BKcolor:24             |
|                            |            |                                     |   |                          |
+----------------------------+            +-------------------------------------+   +--------------------------+

/**
 * @brief 在默认窗口上输出一个字符
 *
 * @param character 字符
 * @param FRcolor 前景色（RGB）
 * @param BKcolor 背景色（RGB）
 * @return int
 */
extern int textui_putchar(uint16_t character, uint32_t FRcolor, uint32_t BKcolor);

/**
 * @brief 初始化text ui框架
 *
 * @return int
 */
extern int textui_init();