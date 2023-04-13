#include "cmd.h"
#include <fcntl.h>
#include <libKeyboard/keyboard.h>
#include <printf.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#define pause_cpu() asm volatile("pause\n\t");
#define MEM_HISTORY 1024
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
// 保存的历史命令(瞬时更改)
char history_commands[MEM_HISTORY][INPUT_BUFFER_SIZE];
// 真正的历史命令
char real_history_commands[MEM_HISTORY][INPUT_BUFFER_SIZE];
int count_history;
// 现在对应的命令
int current_command_index;
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

        // 添加初始光标
        put_string(" ", COLOR_BLACK, COLOR_WHITE);

        // 循环读取每一行到buffer
        count_history++;
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
        }
        if (count)
        {
            char command_origin[strlen(input_buffer)];
            strcpy(command_origin, input_buffer);
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
    // char kb_file_path[] = "/dev/char/ps2_keyboard";

    // int kb_fd = open(kb_file_path, 0);
    print_ascii_logo();
    // printf("before mkdir\n");
    // mkdir("/aaac", 0);
    // printf("after mkdir\n");
    main_loop(0);
    while (1)
        ;
}
/**
 * @brief 清除缓冲区
 *
 * @param count 缓冲区大小
 * @param buf 缓冲区内容
 */
void clear_command(int count, char *buf)
{
    for (int i = 0; i < count; i++)
        printf("%c", '\b');
    memset(buf, 0, sizeof(buf));
}
/**
 * @brief 切换命令(写入到缓冲区)
 *
 * @param buf 缓冲区
 * @param type 如果为1,就向上,如果为-1,就向下
 */
void change_command(char *buf, int type)
{
    current_command_index -= type;
    // 处理边界
    if (current_command_index < 0)
        current_command_index++;
    if (current_command_index >= count_history - 1)
    {
        // 初始只含一条空历史记录，需单独考虑
        if (count_history == 1)
        {
            // 防止出现多条空历史记录
            if (current_command_index > 1)
                current_command_index = 1;
        }
        else
            current_command_index = count_history - 2;
    }

    strcpy(buf, history_commands[current_command_index]);
    printf("%s", buf);
    put_string(" ", COLOR_BLACK, COLOR_WHITE);
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
        // key = keyboard_analyze_keycode(fd);
        key = getchar();
        // printf("key = %d\n", key);
        if (key == 224)
        {
            key = getchar();
            // printf("key = %d\n", key);
            switch (key)
            {
            case 72:
                // 向上方向键
                if (count_history != 0)
                {
                    // put_string(" ", COLOR_WHITE, COLOR_BLACK);
                    printf("%c", '\b');
                    clear_command(count, buf);
                    count = 0;
                    // 向历史
                    change_command(buf, 1);
                    count = strlen(buf);
                }
                key = 0xc8;
                break;
            case 80:
                // 向下方向键
                if (count_history != 0)
                {
                    // put_string(" ", COLOR_WHITE, COLOR_BLACK);
                    printf("%c", '\b');
                    clear_command(count, buf);
                    count = 0;
                    // 向历史
                    change_command(buf, -1);
                    count = strlen(buf);
                }
                key = 0x50;
                break;
            default:
                break;
            }
        }

        if (key == '\n')
        {
            if (count > 0 && current_command_index >= count_history)
            {
                memset(history_commands[current_command_index - 1], 0,
                       sizeof(history_commands[current_command_index - 1]));
                count_history--;
            }
            printf("%c", '\b');
            return count;
        }

        if (key && key != 0x50 && key != 0xc8)
        {
            if (key == '\b')
            {
                if (count > 0)
                {
                    // 回退去除先前光标
                    printf("%c", '\b');
                    // 去除字符
                    printf("%c", '\b');
                    buf[--count] = 0;
                    // 在最后一个字符处加光标
                    put_string(" ", COLOR_BLACK, COLOR_WHITE);
                }
            }
            else
            {
                printf("%c", '\b');
                buf[count++] = key;
                printf("%c", key);
                // 在最后一个字符处加光标
                put_string(" ", COLOR_BLACK, COLOR_WHITE);
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
        {
            printf("%c", '\b');
            return count;
        }

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