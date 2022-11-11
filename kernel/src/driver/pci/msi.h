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
    uint32_t vector_control;
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
 * @brief msi capability list的结构
 *
 */
struct pci_msi_cap_t
{
    uint8_t cap_id;
    uint8_t next_off;
    uint16_t msg_ctrl;

    uint32_t msg_addr_lo;
    uint32_t msg_addr_hi;

    uint16_t msg_data;
    uint16_t Rsvd;

    uint32_t mask;
    uint32_t pending;
};

/**
 * @brief MSI-X的capability list结构体
 * 
 */
struct pci_msix_cap_t
{
    uint8_t cap_id;
    uint8_t next_off;
    uint16_t msg_ctrl;

    uint32_t dword1; // 该DWORD的组成为：[Table Offset][BIR2:0].
                     // 由于Table Offset是8字节对齐的，因此mask掉该dword的BIR部分，就是table offset的值
    uint32_t dword2; // 该DWORD的组成为：[Pending Bit Offset][Pending Bit BIR2:0].
                     // 由于Pending Bit Offset是8字节对齐的，因此mask掉该dword的BIR部分，就是Pending Bit Offset的值
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
    struct pci_msi_desc_t pci;                     // 与pci相关的msi描述符数据
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
int pci_enable_msi(struct msi_desc_t *msi_desc);

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