#include "usb.h"
#include "xhci/xhci.h"
#include <common/kprint.h>
#include <common/errno.h>
#include <driver/pci/pci.h>
#include <debug/bug.h>
#include <common/spinlock.h>

extern spinlock_t xhci_controller_init_lock; // xhci控制器初始化锁

#define MAX_USB_NUM 8 // pci总线上的usb设备的最大数量

// 在pci总线上寻找到的usb设备控制器的header
static struct pci_device_structure_header_t *usb_pdevs[MAX_USB_NUM];
static int usb_pdevs_count = 0;

/**
 * @brief 初始化usb驱动程序
 *
 */
int usb_init(void* unused)
{
    kinfo("Initializing usb driver...");
    spin_init(&xhci_controller_init_lock);

    // 获取所有usb-pci设备的列表
    pci_get_device_structure(USB_CLASS, USB_SUBCLASS, usb_pdevs, &usb_pdevs_count);

    if (WARN_ON(usb_pdevs_count == 0))
    {
        kwarn("There is no usb hardware in this computer!");
        return 0;
    }
    kdebug("usb_pdevs_count=%d", usb_pdevs_count);
    // 初始化每个usb控制器
    for (volatile int i = 0; i < usb_pdevs_count; ++i)
    {
        io_mfence();
        switch (usb_pdevs[i]->ProgIF)
        {
        case USB_TYPE_UHCI:
        case USB_TYPE_OHCI:
        case USB_TYPE_EHCI:
        case USB_TYPE_UNSPEC:
        case USB_TYPE_DEVICE:
            kwarn("Unsupported usb host type: %#02x", usb_pdevs[i]->ProgIF);
            break;

        case USB_TYPE_XHCI:
            // 初始化对应的xhci控制器
            io_mfence();
            xhci_init((struct pci_device_structure_general_device_t *)usb_pdevs[i]);
            io_mfence();
            break;

        default:
            kerror("Error value of usb_pdevs[%d]->ProgIF: %#02x", i, usb_pdevs[i]->ProgIF);
            return -EINVAL;
            break;
        }
    }
    kinfo("Successfully initialized all usb host controllers!");
    return 0;
}