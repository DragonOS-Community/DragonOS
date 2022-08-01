/**
 * @file unistd.h
 * @author fslongjin (longjin@RinGoTek.cn)
 * @brief
 * @version 0.1
 * @date 2022-04-22
 *
 * @copyright Copyright (c) 2022
 *
 */
#pragma once

#include <syscall/syscall.h>
#include <syscall/syscall_num.h>

/**
 * @brief fork当前进程
 *
 * @return pid_t
 */
pid_t fork(void);

/**
 * @brief vfork当前进程
 *
 * @return pid_t
 */
pid_t vfork(void);