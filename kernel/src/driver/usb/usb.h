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
 * @brief usb描述符的头部
 *
 * String Descriptor:
 * String Language Descriptor:
 *      先获取头部，然后根据长度申请空间，再获取整个string desc
 */
struct usb_desc_header
{
    uint8_t len; // 整个描述符的大小（字节）
    uint8_t type;
} __attribute__((packed));

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
} __attribute__((packed));

/**
 * @brief usb设备配置信息描述符
 *
 */
struct usb_config_desc
{
    uint8_t len;            // 当前描述符的大小（字节）
    uint8_t type;           // USB_DT_CONFIG
    uint16_t total_len;     /*
                                Total length of data returned for this
                                configuration. Includes the combined length
                                of all descriptors (configuration, interface,
                                endpoint, and class- or vendor-specific)
                                returned for this configuration
                            */
    uint8_t num_interfaces; // 当前conf对应的接口数量
    uint8_t value;          /*
                                Value to use as an argument to the
                                SetConfiguration() request to select this
                                configuration
                            */
    uint8_t index;          // Index of string descriptor describing this configuration
    uint8_t bmAttr;         /*
                                Configuration characteristics:
                                D7: Reserved (要设置为1)
                                D6: Self-powered
                                D5: Remote Wakeup
                                D4...0: Reserved (设置为0)
                            */
    uint8_t max_power;      /*
                                当这个设备满载时，为在这个conf上提供对应的功能，需要消耗的电流值。
                                当设备是在High-speed时，这里的单位是2mA （也就是说，值为50，代表最大消耗100mA的电流）
                                当设备运行在Gen X speed时，这里的单位是8mA
                            */
} __attribute__((packed));

/**
 * @brief usb接口描述符
 *
 */
struct usb_interface_desc
{
    uint8_t len;
    uint8_t type;                // USB_DT_INTERFACE
    uint8_t interface_number;    // 当前接口序号（从0开始的）
    uint8_t alternate_setting;   // used to select alt. setting
    uint8_t num_endpoints;       // 当前interface的端点数量
    uint8_t interface_class;     // Class code
    uint8_t interface_sub_class; // Sub class code
    uint8_t interface_protocol;  // 协议  These codes are qualified by the value of thebInterfaceClass and the
                                 // bInterfaceSubClass fields.
    uint8_t index;               // index of String Descriptor describing this interface
} __attribute__((packed));

/**
 * @brief usb端点描述符
 *
 * 详见usb3.2 Specification Table 9-26
 */
struct usb_endpoint_desc
{
    uint8_t len;
    uint8_t type;          // descriptor type
    uint8_t endpoint_addr; /*  Bit 3...0: The endpoint number
                               Bit 6...4: Reserved, reset to zero
                               Bit 7: Direction, ignored for
                               control endpoints
                               0 = OUT endpoint
                               1 = IN endpoint
                               */
    uint8_t attributes;
    uint16_t max_packet;
    uint8_t interval;
};

// 从endpoint描述符中获取max burst size大小
#define usb_get_max_burst_from_ep(__ep_desc) (((__ep_desc)->max_packet & 0x1800) >> 11)

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
} __attribute__((packed));
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
#define USB_REQ_TYPE_GET_INTERFACE_REQUEST (__USB_REQ_TYPE_D2H | __USB_REQ_TYPE_STANDARD | __USB_REQ_TYPE_INTERFACE)
#define USB_REQ_TYPE_SET_INTERFACE (__USB_REQ_TYPE_H2D | __USB_REQ_TYPE_STANDARD | __USB_REQ_TYPE_INTERFACE)
#define USB_REQ_TYPE_SET_CLASS_INTERFACE (__USB_REQ_TYPE_H2D | __USB_REQ_TYPE_CLASS | __USB_REQ_TYPE_INTERFACE)

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
    USB_REQ_SET_ENCRYPTION,
    USB_REQ_GET_ENCRYPTION,
    USB_REQ_SET_HANDSHAKE,
    USB_REQ_GET_HANDSHAKE,
    USB_REQ_SET_CONNECTION,
    USB_REQ_SET_SECURITY_DATA,
    USB_REQ_GET_SECURITY_DATA,
    USB_REQ_SET_WUSB_DATA,
    USB_REQ_LOOPBACK_DATA_WRITE,
    USB_REQ_LOOPBACK_DATA_READ,
    USB_REQ_SET_INTERFACE_DS,
    USB_REQ_GET_FW_STATUS = 26,
    USB_REQ_SET_FW_STATUS,
    USB_REQ_SET_SEL = 48,
    USB_REQ_SET_ISOCH_DELAY,
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
    USB_DT_BOS = 15,
    USB_DT_DEVICE_CAPABILITY,

    USB_DT_HID = 0x21,
    USB_DT_HID_REPORT,
    USB_DT_HID_PHYSICAL,

    USB_DT_INTERFACE_FUNCTION = 0x24,
    USB_DT_ENDPOINT_FUNCTION,

    // HUB = 0x29

    USB_DT_SUPERSPEED_USB_ENDPOINT_COMPANION = 48,
    USB_DT_SUPERSPEEDPLUS_ISOCHRONOUS_ENDPOINT_COMPANION,
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
 * @brief 该宏定义用于声明usb请求包，并初始化其中的各个字段
 *
 */
#define DECLARE_USB_PACKET(pak_name, _trans_req_type, _trans_request, _trans_value, _trans_index, _transfer_length)    \
    struct usb_request_packet_t pak_name = {0};                                                                        \
    pak_name.request_type = (_trans_req_type);                                                                         \
    pak_name.request = (_trans_request);                                                                               \
    pak_name.value = (_trans_value);                                                                                   \
    pak_name.index = (_trans_index);                                                                                   \
    pak_name.length = (_transfer_length);

/*
    usb class codes
    refs: https://www.usb.org/defined-class-codes
*/
enum
{
    USB_CLASS_IF = 0x00,
    USB_CLASS_AUDIO,
    USB_CLASS_CDC,
    USB_CLASS_HID,
    USB_CLASS_PHYSICAL = 0x05,
    USB_CLASS_IMAGE,
    USB_CLASS_PRINTER,
    USB_CLASS_MASS_STORAGE,
    USB_CLASS_HUB,
    USB_CLASS_CDC_DATA,
    USB_CLASS_SMART_CARD,
    USB_CLASS_CONTENT_SEC = 0x0d,
    USB_CLASS_VIDEO,
    USB_CLASS_PERSONAL_HEALTHCARE = 0x0f,
    USB_CLASS_AV,
    USB_CLASS_BILLBOARD,
    USB_CLASS_TYPEC_BRIDGE,
    USB_CLASS_I3C = 0x3c,
    USB_CLASS_DIAGNOSTIC = 0xdc,
    USB_CLASS_WIRELESS_CTRL = 0xe0,
    USB_CLASS_MISC = 0xef,
    USB_CLASS_APP_SPEC = 0xfe,
    USB_CLASS_VENDOR_SPEC = 0XFF,
};

/**
 * @brief usb hid descriptor的结构体
 *
 */
struct usb_hid_desc
{
    uint8_t len;
    uint8_t type;    // USB_DT_HID
    uint16_t bcdHID; // 标识HIDClass规范版本的数字表达式。

    uint8_t country_code;
    uint8_t descriptors_num;  //  the number of class descriptors
    uint8_t desc_type;        // Constant name identifying type of class descriptor
    uint16_t report_desc_len; // Report descriptor的大小
};

/**
 * @brief 初始化usb驱动程序
 *
 */
int usb_init();