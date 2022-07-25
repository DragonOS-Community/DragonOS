#pragma once
#include <common/glib.h>

/**
 * @brief msi消息内容结构体
 *
 */
struct msi_msg_t
{
    uint32_t address_lo;
    uint32_t address_hi;
    uint32_t data;
};
struct pci_msi_desc_t
{
    union
    {
        uint32_t msi_mask;  // [PCI MSI]   MSI cached mask bits
        uint32_t msix_ctrl; // [PCI MSI-X] MSI-X cached per vector control bits
    };

    struct
    {
        uint8_t is_msix : 1;  // [PCI MSI/X] True if MSI-X
        uint8_t can_mask : 1; // [PCI MSI/X] Masking supported?
        uint8_t is_64 : 1;    // [PCI MSI/X] Address size: 0=32bit 1=64bit
    } msi_attribute;
};

/**
 * @brief msi描述符
 *
 */
struct msi_desc_t
{
    uint16_t irq_num;                              // 中断向量号
    uint16_t processor;                            // 定向投递的处理器
    uint16_t edge_trigger;                         // 是否边缘触发
    uint16_t assert;                               // 是否高电平触发
    struct pci_device_structure_header_t *pci_dev; // 对应的pci设备的结构体
    struct msi_msg_t msg;                          // msi消息
    uint16_t msi_index;                            // msi描述符的index
    struct pci_msi_desc_t pci;                         // 与pci相关的msi描述符数据
};

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
int pci_enable_msi(struct msi_desc_t * msi_desc);

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