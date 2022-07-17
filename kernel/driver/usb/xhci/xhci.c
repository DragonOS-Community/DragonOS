#include "xhci.h"
#include <common/kprint.h>
#include <debug/bug.h>
#include <process/spinlock.h>

spinlock_t xhci_controller_init_lock; // xhci控制器初始化锁(在usb_init中被初始化)

static int xhci_ctrl_count = 0;    // xhci控制器计数



/**
 * @brief 初始化xhci控制器
 *
 * @param header 指定控制器的pci device头部
 */
void xhci_init(struct pci_device_structure_general_device_t *dev_hdr)
{
    spin_lock(&xhci_controller_init_lock);
    kinfo("Initializing xhci host controller: bus=%#02x, device=%#02x, func=%#02x, VendorID=%#04x, irq_line=%d, irq_pin=%d", dev_hdr->header.bus, dev_hdr->header.device, dev_hdr->header.func, dev_hdr->header.Vendor_ID, dev_hdr->Interrupt_Line, dev_hdr->Interrupt_PIN );

    pci_write_config(dev_hdr->header.bus, dev_hdr->header.device, dev_hdr->header.func, 0x4, 0x0006); // mem I/O access enable and bus master enable


    
    ++xhci_ctrl_count;
    spin_unlock(&xhci_controller_init_lock);
}