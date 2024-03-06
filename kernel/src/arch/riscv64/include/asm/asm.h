#pragma once

#include "DragonOS/stdint.h"
#include <common/stddef.h>
#include <stdbool.h>

// RISC-V 没有直接的开启/关闭中断的指令，你需要通过修改CSR寄存器来实现
// 你可能需要在你的中断处理程序中处理这些操作

#define nop() __asm__ __volatile__("nop\n\t")

// RISC-V 没有 hlt 指令，你可能需要使用 wfi 指令来等待中断
#define hlt() __asm__ __volatile__("wfi\n\t")

// RISC-V 没有 pause 指令，你可能需要使用其他方法来实现处理器等待

// RISC-V 使用 fence 指令来实现内存屏障
#define io_mfence() __asm__ __volatile__("fence rw,rw\n\t" ::: "memory")
#define io_sfence() __asm__ __volatile__("fence w,w\n\t" ::: "memory")
#define io_lfence() __asm__ __volatile__("fence r,r\n\t" ::: "memory")

// 开启中断
#define sti() __asm__ __volatile__("csrsi mstatus, 8\n\t" ::: "memory")

// 关闭中断
#define cli() __asm__ __volatile__("csrci mstatus, 8\n\t" ::: "memory")

// 从io口读入8个bit
unsigned char io_in8(unsigned short port) {
  while (1)
    ;
}

// 从io口读入32个bit
unsigned int io_in32(unsigned short port) {
  while (1)
    ;
}

// 输出8个bit到输出端口
void io_out8(unsigned short port, unsigned char value) {
  while (1)
    ;
}

// 输出32个bit到输出端口
void io_out32(unsigned short port, unsigned int value) {
  while (1)
    ;
}

/**
 * @brief 验证地址空间是否为用户地址空间
 *
 * @param addr_start 地址起始值
 * @param length 地址长度
 * @return true
 * @return false
 */
bool verify_area(uint64_t addr_start, uint64_t length) {
  while (1)
    ;
}