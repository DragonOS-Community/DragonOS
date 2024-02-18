#include "keyboard.h"
#include <unistd.h>

// 功能键标志变量
static bool shift_l = 0, shift_r = 0, ctrl_l = 0, ctrl_r = 0, alt_l = 0, alt_r = 0;
static bool gui_l = 0, gui_r = 0, apps = 0, insert = 0, home = 0, pgup = 0, del = 0, end = 0, pgdn = 0, arrow_u = 0, arrow_l = 0, arrow_d = 0, arrow_r = 0;
static bool kp_forward_slash = 0, kp_en = 0;

// 键盘扫描码有三种：
// 0xE1开头的PauseBreak键
// 0xE0开头的功能键
// 1byte的普通按键

// pause break键的扫描码，没错，它就是这么长
unsigned char pause_break_scan_code[] = {0xe1, 0x1d, 0x45, 0xe1, 0x9d, 0xc5};

// 第一套键盘扫描码 及其对应的字符
uint32_t keycode_map_normal[NUM_SCAN_CODES * MAP_COLS] =
    {
        /*scan-code	unShift		Shift		*/
        /*--------------------------------------------------------------*/
        /*0x00*/ 0,
        0,
        /*0x01*/ 0,
        0, // ESC
        /*0x02*/ '1',
        '!',
        /*0x03*/ '2',
        '@',
        /*0x04*/ '3',
        '#',
        /*0x05*/ '4',
        '$',
        /*0x06*/ '5',
        '%',
        /*0x07*/ '6',
        '^',
        /*0x08*/ '7',
        '&',
        /*0x09*/ '8',
        '*',
        /*0x0a*/ '9',
        '(',
        /*0x0b*/ '0',
        ')',
        /*0x0c*/ '-',
        '_',
        /*0x0d*/ '=',
        '+',
        /*0x0e*/ '\b',
        '\b', // BACKSPACE
        /*0x0f*/ '\t',
        '\t', // TAB

        /*0x10*/ 'q',
        'Q',
        /*0x11*/ 'w',
        'W',
        /*0x12*/ 'e',
        'E',
        /*0x13*/ 'r',
        'R',
        /*0x14*/ 't',
        'T',
        /*0x15*/ 'y',
        'Y',
        /*0x16*/ 'u',
        'U',
        /*0x17*/ 'i',
        'I',
        /*0x18*/ 'o',
        'O',
        /*0x19*/ 'p',
        'P',
        /*0x1a*/ '[',
        '{',
        /*0x1b*/ ']',
        '}',
        /*0x1c*/ '\n',
        '\n', // ENTER
        /*0x1d*/ 0x1d,
        0x1d, // CTRL Left
        /*0x1e*/ 'a',
        'A',
        /*0x1f*/ 's',
        'S',

        /*0x20*/ 'd',
        'D',
        /*0x21*/ 'f',
        'F',
        /*0x22*/ 'g',
        'G',
        /*0x23*/ 'h',
        'H',
        /*0x24*/ 'j',
        'J',
        /*0x25*/ 'k',
        'K',
        /*0x26*/ 'l',
        'L',
        /*0x27*/ ';',
        ':',
        /*0x28*/ '\'',
        '"',
        /*0x29*/ '`',
        '~',
        /*0x2a*/ 0x2a,
        0x2a, // SHIFT Left
        /*0x2b*/ '\\',
        '|',
        /*0x2c*/ 'z',
        'Z',
        /*0x2d*/ 'x',
        'X',
        /*0x2e*/ 'c',
        'C',
        /*0x2f*/ 'v',
        'V',

        /*0x30*/ 'b',
        'B',
        /*0x31*/ 'n',
        'N',
        /*0x32*/ 'm',
        'M',
        /*0x33*/ ',',
        '<',
        /*0x34*/ '.',
        '>',
        /*0x35*/ '/',
        '?',
        /*0x36*/ 0x36,
        0x36, // SHIFT Right
        /*0x37*/ '*',
        '*',
        /*0x38*/ 0x38,
        0x38, // ALT Left
        /*0x39*/ ' ',
        ' ',
        /*0x3a*/ 0,
        0, // CAPS LOCK
        /*0x3b*/ 0,
        0, // F1
        /*0x3c*/ 0,
        0, // F2
        /*0x3d*/ 0,
        0, // F3
        /*0x3e*/ 0,
        0, // F4
        /*0x3f*/ 0,
        0, // F5

        /*0x40*/ 0,
        0, // F6
        /*0x41*/ 0,
        0, // F7
        /*0x42*/ 0,
        0, // F8
        /*0x43*/ 0,
        0, // F9
        /*0x44*/ 0,
        0, // F10
        /*0x45*/ 0,
        0, // NUM LOCK
        /*0x46*/ 0,
        0, // SCROLL LOCK
        /*0x47*/ '7',
        0, /*PAD HONE*/
        /*0x48*/ '8',
        0, /*PAD UP*/
        /*0x49*/ '9',
        0, /*PAD PAGEUP*/
        /*0x4a*/ '-',
        0, /*PAD MINUS*/
        /*0x4b*/ '4',
        0, /*PAD LEFT*/
        /*0x4c*/ '5',
        0, /*PAD MID*/
        /*0x4d*/ '6',
        0, /*PAD RIGHT*/
        /*0x4e*/ '+',
        0, /*PAD PLUS*/
        /*0x4f*/ '1',
        0, /*PAD END*/

        /*0x50*/ '2',
        0, /*PAD DOWN*/
        /*0x51*/ '3',
        0, /*PAD PAGEDOWN*/
        /*0x52*/ '0',
        0, /*PAD INS*/
        /*0x53*/ '.',
        0, /*PAD DOT*/
        /*0x54*/ 0,
        0,
        /*0x55*/ 0,
        0,
        /*0x56*/ 0,
        0,
        /*0x57*/ 0,
        0, // F11
        /*0x58*/ 0,
        0, // F12
        /*0x59*/ 0,
        0,
        /*0x5a*/ 0,
        0,
        /*0x5b*/ 0,
        0,
        /*0x5c*/ 0,
        0,
        /*0x5d*/ 0,
        0,
        /*0x5e*/ 0,
        0,
        /*0x5f*/ 0,
        0,

        /*0x60*/ 0,
        0,
        /*0x61*/ 0,
        0,
        /*0x62*/ 0,
        0,
        /*0x63*/ 0,
        0,
        /*0x64*/ 0,
        0,
        /*0x65*/ 0,
        0,
        /*0x66*/ 0,
        0,
        /*0x67*/ 0,
        0,
        /*0x68*/ 0,
        0,
        /*0x69*/ 0,
        0,
        /*0x6a*/ 0,
        0,
        /*0x6b*/ 0,
        0,
        /*0x6c*/ 0,
        0,
        /*0x6d*/ 0,
        0,
        /*0x6e*/ 0,
        0,
        /*0x6f*/ 0,
        0,

        /*0x70*/ 0,
        0,
        /*0x71*/ 0,
        0,
        /*0x72*/ 0,
        0,
        /*0x73*/ 0,
        0,
        /*0x74*/ 0,
        0,
        /*0x75*/ 0,
        0,
        /*0x76*/ 0,
        0,
        /*0x77*/ 0,
        0,
        /*0x78*/ 0,
        0,
        /*0x79*/ 0,
        0,
        /*0x7a*/ 0,
        0,
        /*0x7b*/ 0,
        0,
        /*0x7c*/ 0,
        0,
        /*0x7d*/ 0,
        0,
        /*0x7e*/ 0,
        0,
        /*0x7f*/ 0,
        0,
};

/**
 * @brief 解析键盘扫描码
 *
 */
int keyboard_analyze_keycode(int fd)
{
    bool flag_make = false;

    int c = keyboard_get_scancode(fd);
    // 循环队列为空
    if (c == -1)
        return 0;

    unsigned char scancode = (unsigned char)c;

    int key = 0;
    if (scancode == 0xE1) // Pause Break
    {
        key = PAUSE_BREAK;
        // 清除缓冲区中剩下的扫描码
        for (int i = 1; i < 6; ++i)
            if (keyboard_get_scancode(fd) != pause_break_scan_code[i])
            {
                key = 0;
                break;
            }
    }
    else if (scancode == 0xE0) // 功能键, 有多个扫描码
    {
        // 获取下一个扫描码
        scancode = keyboard_get_scancode(fd);
        switch (scancode)
        {
        case 0x2a: // print screen 按键被按下
            if (keyboard_get_scancode(fd) == 0xe0)
                if (keyboard_get_scancode(fd) == 0x37)
                {
                    key = PRINT_SCREEN;
                    flag_make = true;
                }
            break;
        case 0xb7: // print screen 按键被松开
            if (keyboard_get_scancode(fd) == 0xe0)
                if (keyboard_get_scancode(fd) == 0xaa)
                {
                    key = PRINT_SCREEN;
                    flag_make = false;
                }
            break;
        case 0x1d: // 按下右边的ctrl
            ctrl_r = true;
            key = OTHER_KEY;
            break;
        case 0x9d: // 松开右边的ctrl
            ctrl_r = false;
            key = OTHER_KEY;
            break;
        case 0x38: // 按下右边的alt
            alt_r = true;
            key = OTHER_KEY;
            break;
        case 0xb8: // 松开右边的alt
            alt_r = false;
            key = OTHER_KEY;
            break;
        case 0x5b:
            gui_l = true;
            key = OTHER_KEY;
            break;
        case 0xdb:
            gui_l = false;
            key = OTHER_KEY;
            break;
        case 0x5c:
            gui_r = true;
            key = OTHER_KEY;
            break;
        case 0xdc:
            gui_r = false;
            key = OTHER_KEY;
            break;
        case 0x5d:
            apps = true;
            key = OTHER_KEY;
            break;
        case 0xdd:
            apps = false;
            key = OTHER_KEY;
            break;
        case 0x52:
            insert = true;
            key = OTHER_KEY;
            break;
        case 0xd2:
            insert = false;
            key = OTHER_KEY;
            break;
        case 0x47:
            home = true;
            key = OTHER_KEY;
            break;
        case 0xc7:
            home = false;
            key = OTHER_KEY;
            break;
        case 0x49:
            pgup = true;
            key = OTHER_KEY;
            break;
        case 0xc9:
            pgup = false;
            key = OTHER_KEY;
            break;
        case 0x53:
            del = true;
            key = OTHER_KEY;
            break;
        case 0xd3:
            del = false;
            key = OTHER_KEY;
            break;
        case 0x4f:
            end = true;
            key = OTHER_KEY;
            break;
        case 0xcf:
            end = false;
            key = OTHER_KEY;
            break;
        case 0x51:
            pgdn = true;
            key = OTHER_KEY;
            break;
        case 0xd1:
            pgdn = false;
            key = OTHER_KEY;
            break;
        case 0x48:
            arrow_u = true;
            key = OTHER_KEY;
            break;
        case 0xc8:
            arrow_u = false;
            key = OTHER_KEY;
            return 0xc8;
            break;
        case 0x4b:
            arrow_l = true;
            key = OTHER_KEY;
            break;
        case 0xcb:
            arrow_l = false;
            key = OTHER_KEY;
            break;
        case 0x50:
            arrow_d = true;
            key = OTHER_KEY;
            return 0x50;
            break;
        case 0xd0:
            arrow_d = false;
            key = OTHER_KEY;
            break;
        case 0x4d:
            arrow_r = true;
            key = OTHER_KEY;
            break;
        case 0xcd:
            arrow_r = false;
            key = OTHER_KEY;
            break;

        case 0x35: // 数字小键盘的 / 符号
            kp_forward_slash = true;
            key = OTHER_KEY;
            break;
        case 0xb5:
            kp_forward_slash = false;
            key = OTHER_KEY;
            break;
        case 0x1c:
            kp_en = true;
            key = OTHER_KEY;
            break;
        case 0x9c:
            kp_en = false;
            key = OTHER_KEY;
            break;

        default:
            key = OTHER_KEY;
            break;
        }
    }

    if (key == 0) // 属于第三类扫描码
    {
        // 判断按键是被按下还是抬起
        flag_make = ((scancode & FLAG_BREAK) ? 0 : 1);

        // 计算扫描码位于码表的第几行
        uint32_t *key_row = &keycode_map_normal[(scancode & 0x7f) * MAP_COLS];
        unsigned char col = 0;
        // shift被按下
        if (shift_l || shift_r)
            col = 1;
        key = key_row[col];

        switch (scancode & 0x7f)
        {
        case 0x2a:
            shift_l = flag_make;
            key = 0;
            break;
        case 0x36:
            shift_r = flag_make;
            key = 0;
            break;
        case 0x1d:
            ctrl_l = flag_make;
            key = 0;
            break;
        case 0x38:
            ctrl_r = flag_make;
            key = 0;
            break;
        default:
            if (!flag_make)
                key = 0;
            break;
        }
        if (key)
            return key;
    }
    return 0;
}

/**
 * @brief 从键盘设备文件中获取键盘扫描码
 *
 */
int keyboard_get_scancode(int fd)
{
    unsigned int ret = 0;
    read(fd, &ret, 1); 
    return ret;
}