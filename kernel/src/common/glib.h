//
// 内核全局通用库
// Created by longjin on 2022/1/22.
//

#pragma once

// 引入对bool类型的支持
#include <DragonOS/stdint.h>
#include <arch/arch.h>
#include <common/compiler.h>
#include <common/stddef.h>
#include <stdbool.h>

#include <asm/asm.h>

/**
 * @brief 根据结构体变量内某个成员变量member的基地址，计算出该结构体变量的基地址
 * @param ptr 指向结构体变量内的成员变量member的指针
 * @param type 成员变量所在的结构体
 * @param member 成员变量名
 *
 * 方法：使用ptr减去结构体内的偏移，得到结构体变量的基地址
 */
#define container_of(ptr, type, member)                                        \
  ({                                                                           \
    typeof(((type *)0)->member) *p = (ptr);                                    \
    (type *)((unsigned long)p - (unsigned long)&(((type *)0)->member));        \
  })

#define ABS(x) ((x) > 0 ? (x) : -(x)) // 绝对值
// 最大最小值
#define max(x, y) ((x > y) ? (x) : (y))
#define min(x, y) ((x < y) ? (x) : (y))

// 遮罩高32bit
#define MASK_HIGH_32bit(x) (x & (0x00000000ffffffffUL))

// 四舍五入成整数
ul round(double x) { return (ul)(x + 0.5); }

/**
 * @brief 地址按照align进行对齐
 *
 * @param addr
 * @param _align
 * @return ul 对齐后的地址
 */
static __always_inline ul ALIGN(const ul addr, const ul _align) {
  return (ul)((addr + _align - 1) & (~(_align - 1)));
}
