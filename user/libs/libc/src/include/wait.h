#pragma once

<<<<<<<< HEAD:user/libs/libc/src/include/sys/wait.h
#include <libc/src/include/sys/types.h>
========
#include <libc/src/include/types.h>
>>>>>>>> 6f52b565fa38d82ee93574ac2059721f34e5fab3:user/libs/libc/src/include/wait.h

/**
 * @brief 等待所有子进程退出
 * 
 * @param stat_loc 返回的子进程结束状态
 * @return pid_t 
 */
pid_t wait(int *stat_loc);

/**
 * @brief 等待指定pid的子进程退出
 * 
 * @param pid 子进程的pid
 * @param stat_loc 返回的子进程结束状态
 * @param options 额外的控制选项
 * @return pid_t 
 */
pid_t waitpid(pid_t pid, int *stat_loc, int options);
