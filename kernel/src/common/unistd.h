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

/**
 * @brief  交换n字节
 *  @param src  源地址
 *  @param dest  目的地址
 * @param nbytes  交换字节数
 */
void swab(void *restrict src, void *restrict dest, ssize_t nbytes);