#include "usb.h"
#include <common/kprint.h>
#include <driver/pci/pci.h>

#define MAX_USB_NUM 8 // pci总线上的usb设备的最大数量

// 在pci总线上寻找到的usb设备控制器的header
struct pci_device_structure_header_t *usb_pdevs[MAX_USB_NUM];

/**
 * @brief 初始化usb驱动程序
 *
 */
void usb_init()
{
    kinfo("Initializing usb driver...");
}