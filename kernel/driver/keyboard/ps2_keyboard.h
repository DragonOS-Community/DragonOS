#pragma once

#include "../../common/glib.h"

#define PS2_KEYBOARD_INTR_VECTOR 0x21   // 键盘的中断向量号

// 定义键盘循环队列缓冲区大小为100bytes
#define ps2_keyboard_buffer_size 100

#define KEYBOARD_CMD_RESET_BUFFER 1

/**
 * @brief 键盘循环队列缓冲区结构体
 *
 */
struct ps2_keyboard_input_buffer
{
    unsigned char *ptr_head;
    unsigned char *ptr_tail;
    int count;
    unsigned char buffer[ps2_keyboard_buffer_size];
};

#define PORT_PS2_KEYBOARD_DATA 0x60
#define PORT_PS2_KEYBOARD_STATUS 0x64
#define PORT_PS2_KEYBOARD_CONTROL 0x64

#define PS2_KEYBOARD_COMMAND_WRITE 0x60 // 向键盘发送配置命令
#define PS2_KEYBOARD_COMMAND_READ 0x20  // 读取键盘的配置值
#define PS2_KEYBOARD_PARAM_INIT 0x47    // 初始化键盘控制器的配置值

// ========= 检测键盘控制器输入/输出缓冲区是否已满
#define PS2_KEYBOARD_FLAG_OUTBUF_FULL 0x01 // 键盘的输出缓冲区已满标志位
#define PS2_KEYBOARD_FLAG_INBUF_FULL 0x02  // 键盘的输入缓冲区已满标志位

// 等待向键盘控制器写入信息完成
#define wait_ps2_keyboard_write() while (io_in8(PORT_PS2_KEYBOARD_STATUS) & PS2_KEYBOARD_FLAG_INBUF_FULL)
// 等待从键盘控制器读取信息完成
#define wait_ps2_keyboard_read() while (io_in8(PORT_PS2_KEYBOARD_STATUS) & PS2_KEYBOARD_FLAG_OUTBUF_FULL)

// 128个按键, 每个按键包含普通按键和shift+普通按键两种状态
#define NUM_SCAN_CODES 0x80
#define MAP_COLS 2

#define PAUSE_BREAK 1
#define PRINT_SCREEN 2
#define OTHER_KEY 4 // 除了上面两个按键以外的功能按键（不包括下面的第三类按键）
#define FLAG_BREAK 0X80

// 键盘扫描码有三种：
// 0xE1开头的PauseBreak键
// 0xE0开头的功能键
// 1byte的普通按键

// pause break键的扫描码，没错，它就是这么长
unsigned char pause_break_scan_code[] = {0xe1, 0x1d, 0x45, 0xe1, 0x9d, 0xc5};

// 第一套键盘扫描码 及其对应的字符
uint keycode_map_normal[NUM_SCAN_CODES*MAP_COLS] = 
{
/*scan-code	unShift		Shift		*/
/*--------------------------------------------------------------*/
/*0x00*/	0,		0,
/*0x01*/	0,		0,		//ESC
/*0x02*/	'1',		'!',
/*0x03*/	'2',		'@',
/*0x04*/	'3',		'#',
/*0x05*/	'4',		'$',
/*0x06*/	'5',		'%',
/*0x07*/	'6',		'^',
/*0x08*/	'7',		'&',
/*0x09*/	'8',		'*',
/*0x0a*/	'9',		'(',
/*0x0b*/	'0',		')',
/*0x0c*/	'-',		'_',
/*0x0d*/	'=',		'+',
/*0x0e*/	'\b',		'\b',		//BACKSPACE	
/*0x0f*/	'\t',		'\t',		//TAB

/*0x10*/	'q',		'Q',
/*0x11*/	'w',		'W',
/*0x12*/	'e',		'E',
/*0x13*/	'r',		'R',
/*0x14*/	't',		'T',
/*0x15*/	'y',		'Y',
/*0x16*/	'u',		'U',
/*0x17*/	'i',		'I',
/*0x18*/	'o',		'O',
/*0x19*/	'p',		'P',
/*0x1a*/	'[',		'{',
/*0x1b*/	']',		'}',
/*0x1c*/	'\n',		'\n',		//ENTER
/*0x1d*/	0x1d,		0x1d,		//CTRL Left
/*0x1e*/	'a',		'A',
/*0x1f*/	's',		'S',

/*0x20*/	'd',		'D',
/*0x21*/	'f',		'F',
/*0x22*/	'g',		'G',
/*0x23*/	'h',		'H',
/*0x24*/	'j',		'J',
/*0x25*/	'k',		'K',
/*0x26*/	'l',		'L',
/*0x27*/	';',		':',
/*0x28*/	'\'',		'"',
/*0x29*/	'`',		'~',
/*0x2a*/	0x2a,		0x2a,		//SHIFT Left
/*0x2b*/	'\\',		'|',
/*0x2c*/	'z',		'Z',
/*0x2d*/	'x',		'X',
/*0x2e*/	'c',		'C',
/*0x2f*/	'v',		'V',

/*0x30*/	'b',		'B',
/*0x31*/	'n',		'N',
/*0x32*/	'm',		'M',
/*0x33*/	',',		'<',
/*0x34*/	'.',		'>',
/*0x35*/	'/',		'?',
/*0x36*/	0x36,		0x36,		//SHIFT Right
/*0x37*/	'*',		'*',
/*0x38*/	0x38,		0x38,		//ALT Left
/*0x39*/	' ',		' ',
/*0x3a*/	0,		0,		//CAPS LOCK
/*0x3b*/	0,		0,		//F1
/*0x3c*/	0,		0,		//F2
/*0x3d*/	0,		0,		//F3
/*0x3e*/	0,		0,		//F4
/*0x3f*/	0,		0,		//F5

/*0x40*/	0,		0,		//F6
/*0x41*/	0,		0,		//F7
/*0x42*/	0,		0,		//F8
/*0x43*/	0,		0,		//F9
/*0x44*/	0,		0,		//F10
/*0x45*/	0,		0,		//NUM LOCK
/*0x46*/	0,		0,		//SCROLL LOCK
/*0x47*/	'7',		0,		/*PAD HONE*/
/*0x48*/	'8',		0,		/*PAD UP*/
/*0x49*/	'9',		0,		/*PAD PAGEUP*/
/*0x4a*/	'-',		0,		/*PAD MINUS*/
/*0x4b*/	'4',		0,		/*PAD LEFT*/
/*0x4c*/	'5',		0,		/*PAD MID*/
/*0x4d*/	'6',		0,		/*PAD RIGHT*/
/*0x4e*/	'+',		0,		/*PAD PLUS*/
/*0x4f*/	'1',		0,		/*PAD END*/

/*0x50*/	'2',		0,		/*PAD DOWN*/
/*0x51*/	'3',		0,		/*PAD PAGEDOWN*/
/*0x52*/	'0',		0,		/*PAD INS*/
/*0x53*/	'.',		0,		/*PAD DOT*/
/*0x54*/	0,		0,
/*0x55*/	0,		0,
/*0x56*/	0,		0,
/*0x57*/	0,		0,		//F11
/*0x58*/	0,		0,		//F12
/*0x59*/	0,		0,		
/*0x5a*/	0,		0,
/*0x5b*/	0,		0,
/*0x5c*/	0,		0,
/*0x5d*/	0,		0,
/*0x5e*/	0,		0,
/*0x5f*/	0,		0,

/*0x60*/	0,		0,
/*0x61*/	0,		0,
/*0x62*/	0,		0,
/*0x63*/	0,		0,
/*0x64*/	0,		0,
/*0x65*/	0,		0,
/*0x66*/	0,		0,
/*0x67*/	0,		0,
/*0x68*/	0,		0,
/*0x69*/	0,		0,
/*0x6a*/	0,		0,
/*0x6b*/	0,		0,
/*0x6c*/	0,		0,
/*0x6d*/	0,		0,
/*0x6e*/	0,		0,
/*0x6f*/	0,		0,

/*0x70*/	0,		0,
/*0x71*/	0,		0,
/*0x72*/	0,		0,
/*0x73*/	0,		0,
/*0x74*/	0,		0,
/*0x75*/	0,		0,
/*0x76*/	0,		0,
/*0x77*/	0,		0,
/*0x78*/	0,		0,
/*0x79*/	0,		0,
/*0x7a*/	0,		0,
/*0x7b*/	0,		0,
/*0x7c*/	0,		0,
/*0x7d*/	0,		0,
/*0x7e*/	0,		0,
/*0x7f*/	0,		0,
};

/**
 * @brief 初始化键盘驱动程序的函数
 *
 */
void ps2_keyboard_init();

/**
 * @brief 键盘驱动卸载函数
 *
 */
void ps2_keyboard_exit();

/**
 * @brief 解析键盘扫描码
 * 
 */
void ps2_keyboard_analyze_keycode();

/**
 * @brief 从缓冲队列中获取键盘扫描码
 * @return 键盘扫描码
 * 若缓冲队列为空则返回-1
 */
int ps2_keyboard_get_scancode();