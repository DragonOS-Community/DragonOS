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

// 将进程的pcb和内核栈融合到一起,8字节对齐
union proc_union
{
    ul stack[STACK_SIZE / sizeof(ul)];
} __attribute__((aligned(8)));

struct tss_struct
{
    unsigned int reserved0;
    ul rsp0;
    ul rsp1;
    ul rsp2;
    ul reserved1;
    ul ist1;
    ul ist2;
    ul ist3;
    ul ist4;
    ul ist5;
    ul ist6;
    ul ist7;
    ul reserved2;
    unsigned short reserved3;
    // io位图基地址
    unsigned short io_map_base_addr;
} __attribute__((packed)); // 使用packed表明是紧凑结构，编译器不会对成员变量进行字节对齐。