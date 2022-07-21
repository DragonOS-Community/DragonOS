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

// Reset wait times(milliseconds) ,USB 2.0 specs, page 153, section 7.1.7.5, paragraph 3
#define USB_TIME_RST_RH 50  //  reset on a root hub
#define USB_TIME_RST_MIN 10 // minimum delay for a reset
#define USB_TIME_RST_NOMORE 3   // No more than this between resets for root hubs
#define USB_TIME_RST_REC 10 // reset recovery

/**
 * @brief 初始化usb驱动程序
 * 
 */
void usb_init();