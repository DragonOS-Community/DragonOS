
/**
 * @file boot_info.h
 * @brief 启动信息接口
 */

#pragma once
#include "glib.h"

/**
 * @brief 启动信息接口
 * 由引导传递的机器信息处理
 * 如 grub2 传递的 multiboot2 结构
 * 注意这部分是通过内存传递的，在重新保存之前不能被覆盖
 * 架构专有的数据在 dtb.h 或 multiboot2.h
 * 实现在 dtb.cpp 或 multiboot2.cpp
 */
    /// 声明，定义在具体的实现中
    /// 地址
    extern uintptr_t  boot_info_addr;
    /// 长度
    extern unsigned int boot_info_size;

