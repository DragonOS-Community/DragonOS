#include "xhci.h"
#include <common/kprint.h>
#include <debug/bug.h>
#include <process/spinlock.h>
#include <mm/mm.h>
#include <debug/traceback/traceback.h>

spinlock_t xhci_controller_init_lock; // xhci控制器初始化锁(在usb_init中被初始化)

static int xhci_ctrl_count = 0; // xhci控制器计数

static struct xhci_host_controller_t xhci_hc[MAX_XHCI_HOST_CONTROLLERS] = {0};

/**
 * @brief 初始化xhci控制器
 *
 * @param header 指定控制器的pci device头部
 */
void xhci_init(struct pci_device_structure_general_device_t *dev_hdr)
{
    spin_lock(&xhci_controller_init_lock);
    kinfo("Initializing xhci host controller: bus=%#02x, device=%#02x, func=%#02x, VendorID=%#04x, irq_line=%d, irq_pin=%d", dev_hdr->header.bus, dev_hdr->header.device, dev_hdr->header.func, dev_hdr->header.Vendor_ID, dev_hdr->Interrupt_Line, dev_hdr->Interrupt_PIN);

    xhci_hc[xhci_ctrl_count].controller_id = xhci_ctrl_count;
    xhci_hc[xhci_ctrl_count].pci_dev_hdr = dev_hdr;
    pci_write_config(dev_hdr->header.bus, dev_hdr->header.device, dev_hdr->header.func, 0x4, 0x0006); // mem I/O access enable and bus master enable

    // 为当前控制器映射寄存器地址空间
    xhci_hc[xhci_ctrl_count].vbase = SPECIAL_MEMOEY_MAPPING_VIRT_ADDR_BASE + XHCI_MAPPING_OFFSET + PAGE_2M_SIZE * xhci_hc[xhci_ctrl_count].controller_id;
    kdebug("dev_hdr->BAR0 & (~0xf)=%#018lx", dev_hdr->BAR0 & (~0xf));
    mm_map_phys_addr(xhci_hc[xhci_ctrl_count].vbase, dev_hdr->BAR0 & (~0xf), 65536, PAGE_KERNEL_PAGE | PAGE_PWT | PAGE_PCD, true);

    // 读取xhci控制寄存器
    uint16_t iversion = *(uint16_t *)(xhci_hc[xhci_ctrl_count].vbase + XHCI_CAPS_HCIVERSION);
    uint32_t dboff = *(uint16_t *)(xhci_hc[xhci_ctrl_count].vbase + XHCI_CAPS_DBOFF);
    
    // kdebug("dboff=%ld", dboff);
    // struct xhci_caps_HCSPARAMS1_reg_t hcs1 = *(struct xhci_caps_HCSPARAMS1_reg_t *)(xhci_hc[xhci_ctrl_count].vbase + XHCI_CAPS_HCSPARAMS1);

    // kdebug("hcs1.max_ports=%d, hcs1.max_slots=%d, hcs1.max_intrs=%d", hcs1.max_ports, hcs1.max_slots, hcs1.max_intrs);
    // kdebug("caps size=%d", *(uint8_t *)xhci_hc[xhci_ctrl_count].vbase);
    // kdebug("iversion=%#06x", iversion);
    if (iversion < 0x95)
    {
        kwarn("Unsupported/Unknowned xHCI controller version: %#06x. This may cause unexpected behavior.", iversion);
    }

    ++xhci_ctrl_count;
    spin_unlock(&xhci_controller_init_lock);
    return;
failed:;
    // 取消地址映射
    mm_unmap(xhci_hc[xhci_ctrl_count].vbase, 65536);

    // 清空数组
    memset((void *)&xhci_hc[xhci_ctrl_count], 0, sizeof(struct xhci_host_controller_t));

    spin_unlock(&xhci_controller_init_lock);
}