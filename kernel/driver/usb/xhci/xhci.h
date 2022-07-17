#pragma once
#include <driver/usb/usb.h>
#include <driver/pci/pci.h>

// xhci Capability Registers offset
#define XHCI_CAPS_CAPLENGTH 0x00 // Cap 寄存器组的长度
#define XHCI_CAPS_RESERVED 0x01
#define XHCI_CAPS_HCIVERSION 0x02 // 接口版本号
#define XHCI_CAPS_HCSPARAMS1 0x04
#define XHCI_CAPS_HCSPARAMS2 0x08
#define XHCI_CAPS_HCSPARAMS3 0x0c
#define XHCI_CAPS_HCCPARAMS1 0x10 // capability params 1
#define XHCI_CAPS_DBOFF 0x14      // Doorbell offset
#define XHCI_CAPS_RTSOFF 0x18     // Runtime register space offset
#define XHCI_CAPS_HCCPARAMS2 0x1c // capability params 2


struct xhci_caps_HCSPARAMS1_reg_t
{
    unsigned max_slots : 8;  // 最大插槽数
    unsigned max_intrs : 11; // 最大中断数
    unsigned reserved : 5;
    unsigned max_ports : 8; // 最大端口数
}__attribute__((packed));


/**
 * @brief xhci端口信息
 *
 */
struct xhci_port_info_t
{
    uint8_t flags;           // port flags
    uint8_t paired_port_num; // 与当前端口所配对的另一个端口（相同物理接口的不同速度的port）
    uint8_t offset;          // offset of this port within this protocal
    uint8_t reserved;
} __attribute__((packed));

struct xhci_controller_t
{
    struct pci_device_structure_general_device_t *pci_dev_hdr; // 指向pci header结构体的指针
    int controller_id;                                         // 操作系统给controller的编号
    int vbase;                                                 // 虚拟地址base（bar0映射到的虚拟地址）
    struct xhci_port_info_t *ports;                            // 指向端口信息数组的指针
};

/**
 * @brief 初始化xhci控制器
 *
 * @param header 指定控制器的pci device头部
 */
void xhci_init(struct pci_device_structure_general_device_t *header);