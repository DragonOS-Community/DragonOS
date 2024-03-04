#include "dmesg.h"
#include <stdio.h>
#include <string.h>

/**
 * @brief 识别dmesg程序的第一个选项参数
 *
 * @param arg dmesg命令第一个选项参数
 * @return int 有效时返回对应选项码，无效时返回 -1
 */
int getoption(char *arg)
{
    if (!strcmp(arg, "-h") || !strcmp(arg, "--help"))
        return 0;
    else if (!strcmp(arg, "-c") || !strcmp(arg, "--read-clear"))
        return 4;
    else if (!strcmp(arg, "-C") || !strcmp(arg, "--clear"))
        return 5;
    else if (!strcmp(arg, "-l") || !strcmp(arg, "--level"))
        return 8;

    return -1;
}

/**
 * @brief 识别dmesg程序的第二个选项参数
 *
 * @param arg dmesg命令第一个选项参数
 * @return int 有效时返回设置的日志级别，无效时返回 -1
 */
int getlevel(char *arg)
{
    if (!strcmp(arg, "EMERG") || !strcmp(arg, "emerg"))
        return 0;
    else if (!strcmp(arg, "ALERT") || !strcmp(arg, "alert"))
        return 1;
    else if (!strcmp(arg, "CRIT") || !strcmp(arg, "crit"))
        return 2;
    else if (!strcmp(arg, "ERR") || !strcmp(arg, "err"))
        return 3;
    else if (!strcmp(arg, "WARN") || !strcmp(arg, "warn"))
        return 4;
    else if (!strcmp(arg, "NOTICE") || !strcmp(arg, "notice"))
        return 5;
    else if (!strcmp(arg, "INFO") || !strcmp(arg, "info"))
        return 6;
    else if (!strcmp(arg, "DEBUG") || !strcmp(arg, "debug"))
        return 7;
    else
    {
        printf("dmesg: unknown level '%s'\n", arg);
    }
    return -2;
}

/**
 * @brief 打印dmesg手册
 */
void print_help_msg()
{
    const char *help_msg = "Usage:\n"
                           " dmesg [options]\n\n"
                           "Display or control the kernel ring buffer.\n\n"
                           "Options:\n"
                           " -C, --clear                 clear the kernel ring buffer\n"
                           " -c, --read-clear            read and clear all messages\n"
                           " -l, --level <list>          restrict output to defined levels\n"
                           " -h, --help                  display this help\n\n"
                           "Supported log levels (priorities):\n"
                           "   emerg - system is unusable\n"
                           "   alert - action must be taken immediately\n"
                           "    crit - critical conditions\n"
                           "     err - error conditions\n"
                           "    warn - warning conditions\n"
                           "  notice - normal but significant condition\n"
                           "    info - informational\n"
                           "   debug - debug-level messages\n";
    printf("%s\n", help_msg);
}

/**
 * @brief 打印dmesg错误使用的信息
 */
void print_bad_usage_msg()
{
    const char *bad_usage_msg = "dmesg: bad usage\nTry 'dmesg --help' for more information.";
    printf("%s\n", bad_usage_msg);
}