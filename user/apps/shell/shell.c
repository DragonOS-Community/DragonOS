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



/**
 * @brief shell主循环
 *
 * @param kb_fd 键盘文件描述符
 */
void main_loop(int kb_fd)
{

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
        int count = shell_readline(kb_fd, input_buffer);

        if (count)
        {
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
    printf("before mkdir\n");
    mkdir("/aaac", 0);
    printf("after mkdir\n");
    main_loop(kb_fd);
    while (1)
        ;
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
        key = keyboard_analyze_keycode(fd);
        if (key == '\n')
            return count;

        if (key)
        {
            if (key == '\b')
            {
                if (count > 0)
                {
                    buf[--count] = 0;
                    printf("%c", '\b');
                }
            }
            else
            {
                buf[count++] = key;

                printf("%c", key);
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