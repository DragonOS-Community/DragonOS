#pragma once

// 128个按键, 每个按键包含普通按键和shift+普通按键两种状态
#define NUM_SCAN_CODES 0x80
#define MAP_COLS 2

#define PAUSE_BREAK 1
#define PRINT_SCREEN 2
#define OTHER_KEY 4 // 除了上面两个按键以外的功能按键（不包括下面的第三类按键）
#define FLAG_BREAK 0X80

/**
 * @brief 解析键盘扫描码
 *
 */
int keyboard_analyze_keycode(char* keycode);