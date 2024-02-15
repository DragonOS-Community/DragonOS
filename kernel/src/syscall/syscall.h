#pragma once

#include <common/glib.h>
#include <common/kprint.h>
#include <common/unistd.h>
#include <process/ptrace.h>

/**
 * @brief 初始化系统调用模块
 *
 */
extern int syscall_init();

/**
 * @brief 用户态系统调用入口函数
 * 从用户态进入系统调用
 * @param syscall_id 系统调用id
 * @return long 错误码
 */
long enter_syscall_int(ul syscall_id, ul arg0, ul arg1, ul arg2, ul arg3, ul arg4, ul arg5);

