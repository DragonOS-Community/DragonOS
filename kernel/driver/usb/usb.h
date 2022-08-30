#pragma once
#include <common/sys/types.h>

// usb设备在pci总线上的class
#define USB_CLASS 0xC
#define USB_SUBCLASS 0x3

// 不同的usb设备在pci总线上的prog IF
#define USB_TYPE_UHCI 0x0
#define USB_TYPE_OHCI 0x10
#define USB_TYPE_EHCI 0x20
#define USB_TYPE_XHCI 0x30
#define USB_TYPE_UNSPEC 0x80 // Unspecified
#define USB_TYPE_DEVICE 0xfe // USB Device(Not controller)

// Reset wait times(milliseconds) ,USB 2.0 specs, page 153, section 7.1.7.5, paragraph 3
#define USB_TIME_RST_RH 50    //  reset on a root hub
#define USB_TIME_RST_MIN 10   // minimum delay for a reset
#define USB_TIME_RST_NOMORE 3 // No more than this between resets for root hubs
#define USB_TIME_RST_REC 10   // reset recovery

/**
 * @brief usb 设备描述符
 *
 */
struct usb_device_desc
{
    uint8_t len;
    uint8_t type;
    uint16_t usb_version;
    uint8_t _class;
    uint8_t subclass;
    uint8_t protocol;
    uint8_t max_packet_size;

    uint16_t vendor_id;
    uint16_t product_id;
    uint16_t device_rel;
    uint8_t manufacturer_index;
    uint8_t procuct_index;

    uint8_t serial_index;
    uint8_t config; // number of configurations
};

/**
 * @brief usb设备请求包
 *
 */
struct usb_request_packet_t
{
    uint8_t request_type;
    uint8_t request;
    uint16_t value;

    uint16_t index;
    uint16_t length;
};
// usb设备请求包的request_type字段的值
#define __USB_REQ_TYPE_H2D 0x00
#define __USB_REQ_TYPE_D2H 0x80

#define __USB_REQ_TYPE_STANDARD 0x00
#define __USB_REQ_TYPE_CLASS 0x20
#define __USB_REQ_TYPE_VENDOR 0x40
#define __USB_REQ_TYPE_RSVD 0x60

#define __USB_REQ_TYPE_DEVICE 0x00
#define __USB_REQ_TYPE_INTERFACE 0x01
#define __USB_REQ_TYPE_ENDPOINT 0x02
#define __USB_REQ_TYPE_OTHER 0x03

#define USB_REQ_TYPE_GET_REQUEST (__USB_REQ_TYPE_D2H | __USB_REQ_TYPE_STANDARD | __USB_REQ_TYPE_DEVICE)
#define USB_REQ_TYPE_SET_REQUEST (__USB_REQ_TYPE_H2D | __USB_REQ_TYPE_STANDARD | __USB_REQ_TYPE_DEVICE)
#define USB_REQ_TYPE_SET_INTERFACE (__USB_REQ_TYPE_H2D | __USB_REQ_TYPE_STANDARD | __USB_REQ_TYPE_INTERFACE)

// device requests
enum
{
    USB_REQ_GET_STATUS = 0,
    USB_REQ_CLEAR_FEATURE,
    USB_REQ_SET_FEATURE = 3,
    USB_REQ_SET_ADDRESS = 5,
    USB_REQ_GET_DESCRIPTOR = 6,
    USB_REQ_SET_DESCRIPTOR,
    USB_REQ_GET_CONFIGURATION,
    USB_REQ_SET_CONFIGURATION,
    // interface requests
    USB_REQ_GET_INTERFACE,
    USB_REQ_SET_INTERFACE,
    // standard endpoint requests
    USB_REQ_SYNCH_FRAME,
    // Device specific
    USB_REQ_GET_MAX_LUNS = 0xFE,
    USB_REQ_BULK_ONLY_RESET
};

// Descriptor types
enum
{
    USB_DT_DEVICE = 1,
    USB_DT_CONFIG,
    USB_DT_STRING,
    USB_DT_INTERFACE,
    USB_DT_ENDPOINT,
    USB_DT_DEVICE_QUALIFIER,
    USB_DT_OTHER_SPEED_CONFIG,
    USB_DT_INTERFACE_POWER,
    USB_DT_OTG,
    USB_DT_DEBUG,
    USB_DT_INTERFACE_ASSOSIATION,

    USB_DT_HID = 0x21,
    USB_DT_HID_REPORT,
    USB_DT_HID_PHYSICAL,

    USB_DT_INTERFACE_FUNCTION = 0x24,
    USB_DT_ENDPOINT_FUNCTION,

    HUB = 0x29
};

// transfer types (Endpoint types) (USB 2.0 page 270)
enum
{
    USB_EP_CONTROL = 0,
    USB_EP_ISOCHRONOUS,
    USB_EP_BULK,
    USB_EP_INTERRUPT
};

/**
 * @brief 初始化usb驱动程序
 *
 */
void usb_init();