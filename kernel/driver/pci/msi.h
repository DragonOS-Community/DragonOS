#pragma once
#include <common/glib.h>


/**
 * @brief 启用 Message Signaled Interrupts
 * 
 * @param header 设备header
 * @param vector 中断向量号
 * @param processor 要投递到的处理器
 * @param edge_trigger 是否边缘触发
 * @param assert 是否高电平触发
 * 
 * @return 返回码
 */
int pci_enable_msi(void * header, uint8_t vector, uint32_t processor, uint8_t edge_trigger, uint8_t assert);

/**
 * @brief 禁用指定设备的msi
 *
 * @param header pci header
 * @return int
 */
int pci_disable_msi(void *header);

/**
 * @brief 在已配置好msi寄存器的设备上，使能msi
 *
 * @param header 设备头部
 * @return int 返回码
 */
int pci_start_msi(void *header);