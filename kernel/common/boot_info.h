
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

    /**
     * @brief 初始化，定义在具体实现中
     * @return true            成功
     * @return false           成功
     */
    extern int init(void);

    /**
     * @brief 获取物理内存信息
     * @return resource_t      物理内存资源信息
     */
    //extern resource_t get_memory(void);

    /**
     * @brief 获取 clint 信息
     * @return resource_t       clint 资源信息
     */
    //extern resource_t get_clint(void);
    
    /**
     * @brief 获取 plic 信息
     * @return resource_t       plic 资源信息
     */
    //extern resource_t get_plic(void);


