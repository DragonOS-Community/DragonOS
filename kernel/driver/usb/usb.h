#pragma once

// usb设备在pci总线上的class
#define USB_CLASS 0xC
#define USB_SUBCLASS 0x3

// 不同的usb设备在pci总线上的prog IF
#define USB_TYPE_UHCI 0x0
#define USB_TYPE_OHCI 0x10
#define USB_TYPE_EHCI 0x20
#define USB_TYPE_XHCI 0x30
#define USB_TYPE_UNSPEC 0x80    // Unspecified
#define USB_TYPE_DEVICE 0xfe    // USB Device(Not controller)

/**
 * @brief 初始化usb驱动程序
 * 
 */
void usb_init();