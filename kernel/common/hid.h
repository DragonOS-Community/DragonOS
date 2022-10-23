#pragma once
#include <common/stddef.h>

#define __HID_USAGE_TABLE_SIZE 64 // usage stack的大小
#define HID_MAX_REPORT 300        // 最大允许的hid report数目（包括feature、input、output）
#define HID_MAX_PATH_SIZE 16      // maximum depth for path

// 这部分请参考hid_1_11.pdf Section 6.2.2.4

#define HID_ITEM_COLLECTION 0xA0
#define HID_ITEM_END_COLLECTION 0xC0
#define HID_ITEM_FEATURE 0xB0
#define HID_ITEM_INPUT 0x80
#define HID_ITEM_OUTPUT 0x90

/**
 * @brief 枚举hid的usage page列表。
 * 原始数据请见<HID Usage Tables FOR Universal Serial Bus (USB)>。
 * 该文件可从usb.org下载
 */
enum HID_USAGE_PAGE_TYPES
{
    HID_USAGE_PAGE_GEN_DESKTOP = 0x1,
    HID_USAGE_PAGE_SIMU_CTRL,               // simulation controls
    HID_USAGE_PAGE_VR_CTRL,                 // vr controls page
    HID_USAGE_PAGE_SPORT_CTRL,              // sport controls
    HID_USAGE_PAGE_GAME_CTRL,               // game controls
    HID_USAGE_PAGE_GEN_DEVICE_CTRL,         // general device controls
    HID_USAGE_PAGE_KBD_KPD,                     // keyboard/ keypad page
    HID_USAGE_PAGE_LED,                     // LED
    HID_USAGE_PAGE_BUTTON,                  // button page
    HID_USAGE_PAGE_ORDINAL,                 // ordinal page
    HID_USAGE_PAGE_TEL_DEVICE,              // telephony device
    HID_USAGE_PAGE_CONSUMER,                // consumer page
    HID_USAGE_PAGE_DIGITIZER,               // digitizers page
    HID_USAGE_PAGE_HAPTICS,                 // haptics page
    HID_USAGE_PAGE_PHY_INPUT_DEVICE,        // physical input device page
    HID_USAGE_PAGE_UNICODE = 0x10,          // unicode page
    HID_USAGE_PAGE_EYE_HEAD_TRACKER = 0x12, // eye and head trackers page
    HID_USAGE_PAGE_AUX_DISPLAY = 0x14,      // auxiliary display page
    HID_USAGE_PAGE_SENSORS = 0x20,          // sensors page
    HID_USAGE_PAGE_MEDICAL = 0x40,          // medical instruments
    HID_USAGE_PAGE_BRAILLE_DISPLAY,         // barille display
    HID_USAGE_PAGE_LIGHTNING_ILLU = 0x59,   // lighting and illumination page
    HID_USAGE_PAGE_MONITOR = 0x80,          // monitor page
    HID_USAGE_PAGE_MONITOR_ENUMERATED,      // monitor enumerated page
    HID_USAGE_PAGE_VESA_VIRT_CTRL,          // VESA virtual controls page
    HID_USAGE_PAGE_POWER = 0x84,            // power page
    HID_USAGE_PAGE_BATTERY_SYSTEM,          // battery system page
    HID_USAGE_PAGE_BARCODE_SCANNER = 0x8c,  // barcode scanner page
    HID_USAGE_PAGE_SCALES,                  // scales page
    HID_USAGE_PAGE_MAGNET_STRIPE_READER,    // magnetic stript reader page
    HID_USAGE_PAGE_CAMERA_CONTROL = 0x90,   // camera control page
    HID_USAGE_PAGE_ARCADE,                  // arcade page
    HID_USAGE_PAGE_GAMING_DEVICE,           // gaming device page
    HID_USAGE_PAGE_FIDO_ALLIANCE = 0xf1d0,  // FIDO alliance page
};

/**
 * @brief usage type for HID_USAGE_PAGE_GEN_DESKTOP page
 *
 */
enum USAGE_TYPE_GENDESK
{
    HID_USAGE_GENDESK_UNDEF = 0, // undefined
    HID_USAGE_GENDESK_POINTER,
    HID_USAGE_GENDESK_MOUSE,
    HID_USAGE_GENDESK_KEYBOARD = 0x6,
    HID_USAGE_GENDESK_POINTER_X = 0x30,
    HID_USAGE_GENDESK_POINTER_Y,
    HID_USAGE_GENDESK_WHEEL = 0x38,
    HID_USAGE_GENDESK_NOTHING = 0xff,
};

/**
 * @brief 描述hid path中的一个节点
 *
 */
struct hid_node_t
{
    int u_page;
    int usage;
};

/**
 * @brief 描述一条hid path
 *
 */
struct hid_path_t
{
    int size; // 路径中的节点数目
    struct hid_node_t node[HID_MAX_PATH_SIZE];
};

/**
 * @brief Describe a HID Data with its location in report
 *
 */
struct hid_data_t
{
    int value;              // hid对象的值
    struct hid_path_t path; // hid path

    int report_count; // count of reports for this usage type
    int offset;       // offset of data in report
    int size;         // size of data in bits

    uint8_t report_id; // report id(from incoming report)
    uint8_t type;      // 数据类型：FEATURE / INPUT / OUTPUT
    uint8_t attribute; // report field attribute. (2 = (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position))
                       //                           (6 = (Data,Var,Rel,No Wrap,Linear,Preferred State,No Null Position))
    int8_t unit_exp;   // unit exponent;

    uint32_t unit; // HID unit

    int logical_min; // Logical min
    int logical_max; // Logical max
    int phys_min;    // Physical min
    int phys_max;    // Physical max
};

/**
 * @brief hid解析器
 *
 */
struct hid_parser
{
    const uint8_t *report_desc; // 指向report descriptor的指针
    int report_desc_size;       // report descriptor的大小（字节）
    int pos;                    // report_desc中，当前正在处理的位置
    uint8_t item;               // 暂存当前的item
    uint32_t value;             // 暂存当前的值

    struct hid_data_t data; // 存储当前的环境

    int offset_table[HID_MAX_REPORT][3]; // 存储 hid report的ID、type、offset
    int report_count;                    // hid report的数量
    int count;                           // local items的计数

    uint32_t u_page;
    struct hid_node_t usage_table[__HID_USAGE_TABLE_SIZE]; // Usage stack
    int usage_size;                                        // usage的数量
    int usage_min;
    int usage_max;

    int cnt_objects; // report descriptor中的对象数目

    int cnt_report; // report desc中的report数目
};

struct hid_usage_types_string
{
    int value;
    const char *string;
};

struct hid_usage_pages_string
{
    int value;
    struct hid_usage_types_string *types;
    const char *string;
};

int hid_parse_report(const void *report_data, const int len);

bool hid_parse_find_object(const void *hid_report, const int report_size, struct hid_data_t *data);