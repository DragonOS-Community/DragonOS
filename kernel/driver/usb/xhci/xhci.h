#pragma once
#include <driver/usb/usb.h>

/**
 * @brief 初始化xhci控制器
 * 
 * @param header 指定控制器的pci device头部
 */
void xhci_init(struct pci_device_structure_header_t *header);