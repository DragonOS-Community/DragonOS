#include <libc/unistd.h>
#include <libc/stdio.h>
#include <libc/fcntl.h>
#include <libc/stdlib.h>
#include <libKeyboard/keyboard.h>
#include <libc/string.h>
#include <libc/stddef.h>

#include "cmd.h"

#define pause_cpu() asm volatile("pause\n\t");

/**
 * @brief 循环读取每一行
 *
 * @param fd 键盘文件描述符
 * @param buf 输入缓冲区
 * @return 读取的字符数
 */
#define INPUT_BUFFER_SIZE 512
int shell_readline(int fd, char *buf);

extern char *shell_current_path;

/**
 * @brief 解析shell命令
 *
 * @param buf 输入缓冲区
 * @param argc 返回值：参数数量
 * @param argv 返回值：参数列表
 * @return int
 */
int parse_command(char *buf, int *argc, char ***argv);

/**
 * @brief shell主循环
 *
 * @param kb_fd 键盘文件描述符
 */
static void main_loop(int kb_fd)
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

/**
 * @brief 解析shell命令
 *
 * @param buf 输入缓冲区
 * @param argc 返回值：参数数量
 * @param argv 返回值：参数列表
 * @return int 主命令的编号
 */
int parse_command(char *buf, int *argc, char ***argv)
{
    // printf("parse command\n");
    int index = 0; // 当前访问的是buf的第几位
    // 去除命令前导的空格
    while (index < INPUT_BUFFER_SIZE && buf[index] == ' ')
        ++index;

    // 计算参数数量
    for (int i = index; i < (INPUT_BUFFER_SIZE - 1); ++i)
    {
        // 到达了字符串末尾
        if (!buf[i])
            break;
        if (buf[i] != ' ' && (buf[i + 1] == ' ' || buf[i + 1] == '\0'))
            ++(*argc);
    }

    // printf("\nargc=%d\n", *argc);

    // 为指向每个指令的指针分配空间
    *argv = (char **)malloc(sizeof(char **) * (*argc));
    memset(*argv, 0, sizeof(char **) * (*argc));
    // 将每个命令都单独提取出来
    for (int i = 0; i < *argc && index < INPUT_BUFFER_SIZE; ++i)
    {
        // 提取出命令，以空格作为分割
        *((*argv) + i) = &buf[index];
        while (index < (INPUT_BUFFER_SIZE - 1) && buf[index] && buf[index] != ' ')
            ++index;
        buf[index++] = '\0';

        // 删除命令间多余的空格
        while (index < INPUT_BUFFER_SIZE && buf[index] == ' ')
            ++index;

        // printf("%s\n", (*argv)[i]);
    }
    // 以第一个命令作为主命令，查找其在命令表中的编号
    return shell_find_cmd(**argv);
}