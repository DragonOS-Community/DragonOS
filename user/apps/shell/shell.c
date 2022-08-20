#include <libc/unistd.h>
#include <libc/stdio.h>
#include <libc/fcntl.h>
#include <libc/stdlib.h>
#include <libKeyboard/keyboard.h>
#include <libc/string.h>
#include <libc/stddef.h>
#include <libc/sys/stat.h>
#include "cmd.h"
#define pause_cpu() asm volatile("pause\n\t");
#define MEM_HISTORY 1024
//为了不破坏键盘驱动而实现自定义，所以自己实现用户层驱动
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
 * @brief 循环读取每一行
 *
 * @param fd 键盘文件描述符
 * @param buf 输入缓冲区
 * @return 读取的字符数
 */

int shell_readline(int fd, char *buf);
void print_ascii_logo();
extern char *shell_current_path;
//保存的历史命令(瞬时更改)
char history_commands[MEM_HISTORY][INPUT_BUFFER_SIZE];
//真正的历史命令
char real_history_commands[MEM_HISTORY][INPUT_BUFFER_SIZE];
int count_history;
//现在对应的命令
int current_command_index;

//现在光标的位置
int pointer_position;
/**
 * @brief shell主循环
 *
 * @param kb_fd 键盘文件描述符
 */
void main_loop(int kb_fd)
{
    count_history = 0;
    current_command_index = 0;
    unsigned char input_buffer[INPUT_BUFFER_SIZE] = {0};

    // 初始化当前工作目录的路径
    shell_current_path = (char *)malloc(3);

    memset(shell_current_path, 0, 3);
    shell_current_path[0] = '/';
    shell_current_path[1] = '\0';
    // shell命令行的主循环
    while (true)
    {
        int argc = 0;
        char **argv;

        printf("[DragonOS] %s # ", shell_current_path);

        memset(input_buffer, 0, INPUT_BUFFER_SIZE);

        // 循环读取每一行到buffer
        count_history++;
        pointer_position = -1;
        put_string(" ", COLOR_BLACK, COLOR_WHITE);
        int count = shell_readline(kb_fd, input_buffer);
        if (!count || current_command_index < count_history - 1)
            count_history--;
        if (count)
        {
            strcpy(real_history_commands[count_history - 1], input_buffer);
            count_history++;
            memset(history_commands, 0, sizeof(history_commands));
            for (int i = 0; i <= count_history - 2; i++)
                strcpy(history_commands[i], real_history_commands[i]);
            current_command_index = count_history - 1;
            int cmd_num = parse_command(input_buffer, &argc, &argv);
            printf("\n");
            if (cmd_num >= 0)
                shell_run_built_in_command(cmd_num, argc, argv);
        }
        else
            printf("\n");
    }
}

int main()
{
    // 打开键盘文件
    char kb_file_path[] = "/dev/keyboard.dev";
    int kb_fd = open(kb_file_path, 0);
    // printf("keyboard fd = %d\n", kb_fd);
    print_ascii_logo();
    // printf("before mkdir\n");
    // mkdir("/aaac", 0);
    // printf("after mkdir\n");
    main_loop(kb_fd);
    while (1)
        ;
}
/**
 * @brief 清理但不刷新缓冲区
 *
 * @param count 缓冲区大小
 */
void clear_noclear_buf(int count)
{
    //这里使用"\b \b"的原因是\b不会刷新背景颜色，所以先退掉字符，在用普通空格覆盖，再去掉空格
    for (int i = 0; i < count; i++)
        printf("\b \b");
    if (pointer_position == count - 1)
        printf("\b \b");
}
/**
 * @brief 清除缓冲区
 *
 * @param count 缓冲区大小
 * @param buf 缓冲区内容
 */
void clear_command(int count, char *buf)
{
    clear_noclear_buf(count);
    memset(buf, 0, sizeof(buf));
}
/**
 * @brief 切换命令(写入到缓冲区)
 *
 * @param buf 缓冲区
 * @param type 如果为1,就向下,如果为-1,就向上
 */
void change_command(char *buf, int type)
{
    current_command_index -= type;
    //处理边界
    if (current_command_index < 0)
        current_command_index++;
    if (current_command_index >= count_history - 1)
        current_command_index = count_history - 2;
    strcpy(buf, history_commands[current_command_index]);
}
/**
 * @brief 输出命令（带有光标）
 *
 * @param buf 缓冲区
 * @param count 缓冲区大小
 */
void print_with_pointer(char *buf, int count)
{
    for (int i = 0; i <= pointer_position; i++)
        printf("%c", buf[i]);
    //这里要开大一点，不然有问题
    char x[4];
    memset(x, 0, sizeof(x));
    x[0] = ' ';
    if (pointer_position != count - 1)
    {
        x[0] = buf[pointer_position + 1];
        //黑底白字，显示光标
        put_string(x, COLOR_BLACK, COLOR_WHITE);
    }
    else
        put_string(" ", COLOR_BLACK, COLOR_WHITE);
    for (int i = pointer_position + 2; i < count; i++)
        printf("%c", buf[i]);
}
/**
 * @brief 循环读取每一行
 *
 * @param fd 键盘文件描述符
 * @param buf 输入缓冲区
 * @return 读取的字符数
 */
int shell_readline(int fd, char *buf)
{
    int key = 0;
    int count = 0;
    while (1)
    {
        int scan1 = keyboard_get_scancode(fd);
        int scan2 = -1;
        if (scan1 == 0xE0)
            scan2 = keyboard_get_scancode(fd);
        //向上方向键
        if (count_history != 0 && scan2 == 0x48)
        {
            clear_command(count, buf);
            count = 0;
            //向历史
            change_command(buf, 1);
            count = strlen(buf);
            pointer_position = count - 1;
            print_with_pointer(buf, count);
        }
        //向下方向键
        if (count_history != 0 && scan2 == 0x50)
        {
            clear_command(count, buf);
            count = 0;
            //向现在
            change_command(buf, -1);
            count = strlen(buf);
            pointer_position = count - 1;
            print_with_pointer(buf, count);
        }
        //左方向键
        if (scan2 == 0x4d)
        {
            clear_noclear_buf(count);
            pointer_position++;
            if (pointer_position >= count)
                pointer_position = count - 1;
            print_with_pointer(buf, count);
        }
        //右方向键
        if (scan2 == 0x4b)
        {
            clear_noclear_buf(count);
            pointer_position--;
            if (pointer_position < -1)
                pointer_position = -1;
            print_with_pointer(buf, count);
        }
        //详见keyboard.c
        bool flag_make = false, shift_l = false, shift_r = false, ctrl_l = false, ctrl_r = false;
        unsigned char scancode = (unsigned char)scan1;
        if (scan2 == -1)
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
        }
        if (key == '\n')
        {
            //去掉光标
            clear_noclear_buf(count);
            printf("%s", buf);
            if (count > 0 && current_command_index >= count_history)
            {
                memset(history_commands[current_command_index - 1], 0, sizeof(history_commands[current_command_index - 1]));
                count_history--;
            }
            return count;
        }
        if (key)
        {
            if (key == '\b')
            {
                if (count > 0)
                {
                    if (pointer_position != -1)
                    {
                        clear_noclear_buf(count);
                        //将所有的向左移动一个位置，移动过去
                        buf[pointer_position] = 0;
                        for (int i = pointer_position + 1; i <= count - 1; i++)
                            buf[i - 1] = buf[i];
                        //处理最后一个残留
                        buf[count - 1] = 0;
                        pointer_position--;
                        count--;
                        //显示
                        print_with_pointer(buf, count);
                    }
                }
            }
            else
            {
                clear_noclear_buf(count);
                //与上面的大致相同，这回是向右移动，腾出位置
                for (int i = count - 1; i >= pointer_position + 1; i--)
                    buf[i + 1] = buf[i];
                buf[pointer_position + 1] = key;
                pointer_position++;
                count++;
                print_with_pointer(buf, count);
            }
            if (count > 0 && current_command_index >= count_history)
            {
                memset(history_commands[count_history], 0, sizeof(history_commands[count_history]));
                strcpy(history_commands[count_history], buf);
            }
            else if (count > 0)
            {
                memset(history_commands[current_command_index], 0, sizeof(history_commands[current_command_index]));
                strcpy(history_commands[current_command_index], buf);
            }
        }

        // 输入缓冲区满了之后，直接返回
        if (count >= INPUT_BUFFER_SIZE - 1)
            return count;

        pause_cpu();
    }
}

void print_ascii_logo()
{
    printf("\n\n");
    printf(" ____                                      ___   ____ \n");
    printf("|  _ \\  _ __   __ _   __ _   ___   _ __   / _ \\ / ___| \n");
    printf("| | | || '__| / _` | / _` | / _ \\ | '_ \\ | | | |\\___ \\  \n");
    printf("| |_| || |   | (_| || (_| || (_) || | | || |_| | ___) |\n");
    printf("|____/ |_|    \\__,_| \\__, | \\___/ |_| |_| \\___/ |____/ \n");
    printf("                     |___/     \n");
    printf("\n\n");
}