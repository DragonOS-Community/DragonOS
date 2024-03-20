/**
 * @file sched-wrapper.h
 * @author longjin (longjin@RinGoTek.cn)
 * @brief 这是为调度器相关接口创建rust绑定的wrapper
 * @version 0.1
 * @date 2022-11-10
 *
 * @copyright Copyright (c) 2022
 *
 */
#pragma once


#include <common/glib.h>
#include <common/printk.h>
#include <common/spinlock.h>
#include <common/stdio.h>
#include <common/string.h>
#include <common/time.h>
#include <common/unistd.h>
#include <driver/multiboot2/multiboot2.h>
#include <libs/lib_ui/textui.h>
#include <mm/mm.h>
#include <mm/mmio.h>
#include <mm/slab.h>
#include <process/process.h>
#include <sched/sched.h>
#include <smp/smp.h>
#include <time/clocksource.h>
#include <time/sleep.h>
#include <common/errno.h>
#include <common/cpu.h>
