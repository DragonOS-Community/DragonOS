#pragma once
#include <common/stddef.h>

#define __HID_USAGE_TABLE_SIZE 64 // usage stack的大小
#define HID_MAX_REPORT 300        // 最大允许的hid report数目（包括feature、input、output）
#define HID_MAX_PATH_SIZE 16      // maximum depth for path

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
    uint8_t attribute;  // report field attribute. (2 = (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position))
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

    int cnt_report;   // report desc中的report数目

};


struct hid_usage_types_string
{
    int value;
    const char *string;
};

struct hid_usage_pages_string
{
    int value;
    struct hid_usage_types_string * types;
    const char * string;
};

int hid_parse_report(const void *report_data, const int len);
