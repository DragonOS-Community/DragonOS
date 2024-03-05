#include "dmesg.h"
#include <stdio.h>
#include <stdlib.h>
#include <sys/klog.h>
#include <unistd.h>

int main(int argc, char **argv)
{
    unsigned int len = 1;
    char *buf = NULL;
    int opt;
    unsigned int color = 65280;

    // 获取内核缓冲区大小
    len = klogctl(10, buf, len);

    if (len < 16 * 1024)
        len = 16 * 1024;
    if (len > 16 * 1024 * 1024)
        len = 16 * 1024 * 1024;

    buf = malloc(len);
    if (buf == NULL)
    {
        perror("");
        return -1;
    }

    if (argc == 1)
    {
        // 无选项参数，默认打印所有日志消息
        len = klogctl(2, buf, len);
    }
    else
    {
        // 获取第一个选项参数
        opt = getoption(argv[1]);

        // 无效参数
        if (opt == -1)
        {
            print_bad_usage_msg();
            return -1;
        }
        // 打印帮助手册
        else if (opt == 0)
        {
            print_help_msg();
            return 0;
        }
        // 4 -> 读取内核缓冲区后，清空缓冲区
        // 5 -> 清空内核缓冲区
        else if (opt == 4 || opt == 5)
        {
            len = klogctl(opt, buf, len);
        }
        // 读取特定日志级别的消息
        else if (opt == 8)
        {
            // 无指定日志级别参数，打印错误使用信息
            if (argc < 3)
            {
                print_bad_usage_msg();
                return -1;
            }

            int level = -1;

            // 获取日志级别
            // 这里加1的原因是：如果klogctl的第三个参数是0，不会发生系统调用
            level = getlevel(argv[2]) + 1;

            if (level == -1)
                return -1;

            klogctl(8, buf, level);
            len = klogctl(2, buf, len);
        }
    }

    // 当前打印内容
    // 0: 日志级别
    // 1: 时间戳
    // 2: 代码行号
    // 3: 日志消息
    unsigned int content = 0;
    for (int i = 0; i < len; i++)
    {
        char c[2];
        c[0] = buf[i];
        c[1] = '\0';
        syscall(100000, &c[0], color, 0);
        if (content == 0 && buf[i] == '>')
        {
            content++;
        }
        else if (content == 1 && buf[i] == ']')
        {
            color = 16744448;
            content++;
        }
        else if (content == 2 && buf[i] == ')')
        {
            color = 16777215;
            content++;
        }
        else if (content == 3 && buf[i] == '\n')
        {
            color = 65280;
            content = 0;
        }
    }

    free(buf);

    return 0;
}