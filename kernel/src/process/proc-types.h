#pragma once

#include "ptrace.h"
#include <DragonOS/signal.h>
#include <DragonOS/stdint.h>

// 进程最大可拥有的文件描述符数量
#define PROC_MAX_FD_NUM 16

// 进程的内核栈大小 32K
#define STACK_SIZE 32768

// 进程的运行状态
// 正在运行
#define PROC_RUNNING (1 << 0)
// 可被信号打断
#define PROC_INTERRUPTIBLE (1 << 1)
// 不可被信号打断
#define PROC_UNINTERRUPTIBLE (1 << 2)
// 挂起
#define PROC_ZOMBIE (1 << 3)
// 已停止
#define PROC_STOPPED (1 << 4)
