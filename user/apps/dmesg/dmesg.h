#pragma once

/**
 * @brief 识别dmesg程序的第一个选项参数
 *
 * @param arg dmesg命令第一个选项参数
 * @return int 有效时返回对应选项码，无效时返回 -1
 */
int getoption(char *arg);

/**
 * @brief 识别dmesg程序的第二个选项参数
 *
 * @param arg dmesg命令第一个选项参数
 * @return int 有效时返回设置的日志级别，无效时返回 -1
 */
int getlevel(char *arg);

/**
 * @brief 打印dmesg手册
 */
void print_help_msg();

/**
 * @brief 打印dmesg错误使用的信息
 */
void print_bad_usage_msg();