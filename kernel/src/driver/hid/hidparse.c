#include "internal.h"
#include <common/compiler.h>
#include <common/glib.h>
#include <common/hid.h>
#include <common/printk.h>
#include <common/string.h>
#include <debug/bug.h>

/*
    参考文档：https://www.usb.org/document-library/device-class-definition-hid-111
    本文件参考了FYSOS：  https://github.com/fysnet/FYSOS.git
 */

static bool HID_PARSE_OUTPUT = true; // 是否输出解析信息
static char __tmp_usage_page_str[128] = {0};

static void hid_reset_parser(struct hid_parser *parser);

static const char *hid_get_usage_page_str(const int u_page);
static const char *hid_get_usage_type_str(const int page, const int type);
static const char *hid_get_collection_str(const int value);
static int *__get_report_offset(struct hid_parser *parser, const uint8_t report_id, const uint8_t report_type);
static __always_inline const struct hid_usage_pages_string *hid_get_usage_page(const int u_page);

static __always_inline const struct hid_usage_types_string *hid_get_usage_type(
    const struct hid_usage_pages_string *upage, const int type);

// hid item的低2位为size
#define HID_SIZE_MASK 0x3
// 高6bit为item内容
#define HID_ITEM_MASK 0xFC
#define HID_ITEM_UPAGE 0x04 // usage page
#define HID_ITEM_USAGE 0x08 // local item
#define HID_ITEM_LOG_MIN 0x14
#define HID_ITEM_USAGE_MIN 0x18 // local item
#define HID_ITEM_LOG_MAX 0x24
#define HID_ITEM_USAGE_MAX 0x28 // local item
#define HID_ITEM_PHY_MIN 0x34
#define HID_ITEM_PHY_MAX 0x44
#define HID_ITEM_UNIT_EXP 0x54
#define HID_ITEM_UNIT 0x64
#define HID_ITEM_REP_SIZE 0x74
#define HID_ITEM_STRING 0x78 // local item?
#define HID_ITEM_REP_ID 0x84
#define HID_ITEM_REP_COUNT 0x94

static char __spaces_buf[33];
char *__spaces(uint8_t cnt)
{
    static char __space_overflow_str[] = "**";
    if (cnt > 32)
    {
        return __space_overflow_str;
    }

    memset(__spaces_buf, ' ', 32);
    __spaces_buf[cnt] = '\0';
    return __spaces_buf;
}

static __always_inline uint32_t __format_value(uint32_t value, uint8_t size)
{
    switch (size)
    {
    case 1:
        value = (uint32_t)(uint8_t)value;
        break;
    case 2:
        value = (uint32_t)(uint16_t)value;
        break;
    }
    return value;
}

/**
 * @brief 重置parser
 *
 * @param parser 解析器
 * @return int 状态码
 */
static void hid_reset_parser(struct hid_parser *parser)
{
    memset(parser, 0, sizeof(struct hid_parser));
    parser->data.report_id = 1; // we must give it a non-zero value or the parser doesn't work
}

/**
 * @brief 从usage_stack中弹出第一个元素
 *
 * @param parser 解析器
 * @return __always_inline
 */
static __always_inline void __pop_usage_stack(struct hid_parser *parser)
{
    if (parser->usage_size > 0)
    {
        for (int js = 0; js < parser->usage_size - 1; ++js)
            memmove(&parser->usage_table[js], &parser->usage_table[js + 1], sizeof(struct hid_node_t));

        --parser->usage_size;
    }
}

/**
 * @brief 解析hid report，并获取下一个数据到data字段中
 * todo:(不知道为什么，在qemu上面，发现键盘的usage都是0xff)
 * 
 * @param parser 解析器
 * @param data 返回的数据
 * @return true 解析成功
 * @return false 解析失败
 */
static bool hid_parse(struct hid_parser *parser, struct hid_data_t *data)
{
    bool found = false;
    static uint8_t space_cnt = 0;
    static bool did_collection = false;
    static int item_size[4] = {0, 1, 2, 4};

    // 循环解析
    while (!found && (parser->pos < parser->report_desc_size))
    {
        // 当前parse过程还没有解析到report
        if (parser->count == 0)
        {
            // 打印当前 report_data 的值
            if (HID_PARSE_OUTPUT)
                printk("\n %02X ", parser->report_desc[parser->pos]);
            // 获取到report size
            parser->item = parser->report_desc[parser->pos++];
            parser->value = 0;
            // 拷贝report的数据
            memcpy(&parser->value, &parser->report_desc[parser->pos], item_size[parser->item & HID_SIZE_MASK]);

            if (HID_PARSE_OUTPUT)
            {
                for (int i = 0; i < 4; ++i)
                {
                    if (i < item_size[parser->item & HID_SIZE_MASK])
                        printk("%02X ", parser->report_desc[parser->pos + i]);
                    else
                        printk("   ");
                }
            }
            // 将指针指向下一个item
            parser->pos += item_size[parser->item & HID_SIZE_MASK];
        }

        switch (parser->item & HID_ITEM_MASK)
        {
        case HID_ITEM_UPAGE:
            // 拷贝upage
            parser->u_page = (int)parser->value;
            if (HID_PARSE_OUTPUT)
                printk("%sUsage Page (%s)", __spaces(space_cnt), hid_get_usage_page_str(parser->u_page));
            // 拷贝到 usage table。由于这是一个USAGE entry，因此不增加usage_size(以便后面覆盖它)
            parser->usage_table[parser->usage_size].u_page = parser->u_page;
            parser->usage_table[parser->usage_size].usage = 0xff;
            break;
        case HID_ITEM_USAGE:
            // 拷贝upage到usage table中
            if ((parser->item & HID_SIZE_MASK) > 2) // item大小为32字节
                parser->usage_table[parser->usage_size].u_page = (int)(parser->value >> 16);
            else
                parser->usage_table[parser->usage_size].u_page = parser->u_page;

            if (HID_PARSE_OUTPUT)
                printk("%sUsage (%s)", __spaces(space_cnt),
                       hid_get_usage_type_str(parser->u_page, parser->value & 0xffff));
            ++parser->usage_size;
            break;
        case HID_ITEM_USAGE_MIN:
            // todo: 设置usage min
            if (HID_PARSE_OUTPUT)
                printk("%sUsage min (%i=%s)", __spaces(space_cnt), parser->value,
                       hid_get_usage_type_str(parser->u_page, parser->value));
            break;
        case HID_ITEM_USAGE_MAX:
            // todo: 设置usage max
            if (HID_PARSE_OUTPUT)
                printk("%sUsage max (%i=%s)", __spaces(space_cnt), parser->value,
                       hid_get_usage_type_str(parser->u_page, parser->value));
            break;
        case HID_ITEM_COLLECTION:
            // 从usage table中取出第一个u_page和usage，并且将他们存储在parser->data.path
            parser->data.path.node[parser->data.path.size].u_page = parser->usage_table[0].u_page;
            parser->data.path.node[parser->data.path.size].usage = parser->usage_table[0].usage;
            ++parser->data.path.size;

            // 由于上面取出了元素，因此将队列往前移动1个位置
            __pop_usage_stack(parser);

            // 获取index(如果有的话)???
            if (parser->value >= 0x80)
            {
                kdebug("parser->value > 0x80");
                parser->data.path.node[parser->data.path.size].u_page = 0xff;
                parser->data.path.node[parser->data.path.size].usage = parser->value & 0x7f;
                ++parser->data.path.size;
            }
            if (HID_PARSE_OUTPUT)
            {
                printk("%sCollection (%s)", __spaces(space_cnt), hid_get_collection_str(parser->value));
                space_cnt += 2;
            }
            break;
        case HID_ITEM_END_COLLECTION:
            --parser->data.path.size; // 为什么要--？？？？？
            // 删除多余的(未识别的）node
            if (parser->data.path.node[parser->data.path.size].u_page == 0xff)
                --parser->data.path.size;
            if (HID_PARSE_OUTPUT)
            {
                if (space_cnt >= 2)
                    space_cnt -= 2;
                printk("%sEnd Collection", __spaces(space_cnt));
            }
            break;
        case HID_ITEM_FEATURE:
        case HID_ITEM_INPUT:
        case HID_ITEM_OUTPUT:
            // 找到了一个对象
            found = true;

            // 增加对象计数器
            ++parser->cnt_objects;

            // 更新local items的计数
            if (parser->count == 0)
                parser->count = parser->report_count;

            // 从usage_table获取u_page和usage，将他们存储到parser.data.path
            parser->data.path.node[parser->data.path.size].u_page = parser->usage_table[0].u_page;
            parser->data.path.node[parser->data.path.size].usage = parser->usage_table[0].usage;
            ++parser->data.path.size;

            // 从usage table中弹出刚刚那个node
            __pop_usage_stack(parser);

            // 拷贝数据到data
            parser->data.type = (uint8_t)(parser->item & HID_ITEM_MASK);
            parser->data.attribute = (uint8_t)parser->value;
            int *offset_ptr =
                __get_report_offset(parser, parser->data.report_id, (uint8_t)(parser->item & HID_ITEM_MASK));

            if (unlikely(offset_ptr == NULL))
            {
                BUG_ON(1);
                return false;
            }
            parser->data.offset = *offset_ptr;

            // 获取pData中的对象
            memcpy(data, &parser->data, sizeof(struct hid_data_t));

            // 增加report offset
            *offset_ptr = (*offset_ptr) + parser->data.size;

            // 从path中删除最后一个节点（刚刚弹出的这个节点）
            --parser->data.path.size;

            // 减少local items计数
            if (parser->count > 0)
                --parser->count;

            if (!did_collection)
            {
                if (HID_PARSE_OUTPUT)
                {
                    if ((parser->item & HID_ITEM_MASK) == HID_ITEM_FEATURE)
                        printk("%sFeature ", __spaces(space_cnt));
                    else if ((parser->item & HID_ITEM_MASK) == HID_ITEM_INPUT)
                        printk("%sInput ", __spaces(space_cnt));
                    else if ((parser->item & HID_ITEM_MASK) == HID_ITEM_OUTPUT)
                        printk("%sOutut ", __spaces(space_cnt));

                    printk("(%s,%s,%s" /* ",%s,%s,%s,%s" */ ")", !(parser->value & (1 << 0)) ? "Data" : "Constant",
                           !(parser->value & (1 << 1)) ? "Array" : "Variable",
                           !(parser->value & (1 << 2)) ? "Absolute" : "Relative" /*,
                              !(parser->value & (1<<3)) ? "No Wrap"  : "Wrap",
                              !(parser->value & (1<<4)) ? "Linear"   : "Non Linear",
                              !(parser->value & (1<<5)) ? "Preferred State" : "No Preferred",
                              !(parser->value & (1<<6)) ? "No Null"  : "Null State",
                              //!(parser->value & (1<<8)) ? "Bit Fueld" : "Buffered Bytes"
                              */
                    );
                }

                did_collection = true;
            }
            break;
        case HID_ITEM_REP_ID: // 当前item表示report id
            parser->data.report_id = (uint8_t)parser->value;
            if (HID_PARSE_OUTPUT)
                printk("%sReport ID: %i", __spaces(space_cnt), parser->data.report_id);
            break;
        case HID_ITEM_REP_SIZE: // 当前item表示report size
            parser->data.size = parser->value;
            if (HID_PARSE_OUTPUT)
                printk("%sReport size (%i)", __spaces(space_cnt), parser->data.size);
            break;
        case HID_ITEM_REP_COUNT:
            parser->report_count = parser->value;
            if (HID_PARSE_OUTPUT)
                printk("%sReport count (%i)", __spaces(space_cnt), parser->report_count);
            break;
        case HID_ITEM_UNIT_EXP:
            parser->data.unit_exp = (int8_t)parser->value;
            if (parser->data.unit_exp > 7)
                parser->data.unit_exp |= 0xf0;
            if (HID_PARSE_OUTPUT)
                printk("%sUnit Exp (%i)", __spaces(space_cnt), parser->data.unit_exp);
            break;
        case HID_ITEM_UNIT:
            parser->data.unit = parser->value;
            if (HID_PARSE_OUTPUT)
                printk("%sUnit (%i)", __spaces(space_cnt), parser->data.unit);
            break;
        case HID_ITEM_LOG_MIN: // logical min
            parser->data.logical_min = __format_value(parser->value, item_size[parser->item & HID_SIZE_MASK]);
            if (HID_PARSE_OUTPUT)
                printk("%sLogical Min (%i)", __spaces(space_cnt), parser->data.logical_min);
            break;
        case HID_ITEM_LOG_MAX:
            parser->data.logical_max = __format_value(parser->value, item_size[parser->item & HID_SIZE_MASK]);
            if (HID_PARSE_OUTPUT)
                printk("%sLogical Max (%i)", __spaces(space_cnt), parser->data.logical_max);
            break;
        case HID_ITEM_PHY_MIN:
            parser->data.phys_min = __format_value(parser->value, item_size[parser->item & HID_SIZE_MASK]);
            if (HID_PARSE_OUTPUT)
                printk("%Physical Min (%i)", __spaces(space_cnt), parser->data.phys_min);
            break;
        case HID_ITEM_PHY_MAX:
            parser->data.phys_max = __format_value(parser->value, item_size[parser->item & HID_SIZE_MASK]);
            if (HID_PARSE_OUTPUT)
                printk("%Physical Max (%i)", __spaces(space_cnt), parser->data.phys_max);
            break;
        default:
            printk("\n Found unknown item %#02X\n", parser->item & HID_ITEM_MASK);
            return found;
        }
    }
    return found;
}

/**
 * @brief 解析hid report的数据
 *
 * @param report_data 从usb hid设备获取到hid report
 * @param len report_data的大小（字节）
 * @return int错误码
 */
int hid_parse_report(const void *report_data, const int len)
{
    struct hid_parser parser = {0};
    struct hid_data_t data;

    hid_reset_parser(&parser);
    parser.report_desc = (const uint8_t *)report_data;
    parser.report_desc_size = len;

    while (hid_parse(&parser, &data))
        ;
    return 0;
}

/**
 * @brief 根据usage page的id获取usage page string结构体.当u_page不属于任何已知的id时，返回NULL
 *
 * @param u_page usage page id
 * @return const struct hid_usage_pages_string * usage page string结构体
 */
static __always_inline const struct hid_usage_pages_string *hid_get_usage_page(const int u_page)
{
    int i = 0;
    while ((hid_usage_page_strings[i].value < u_page) && (hid_usage_page_strings[i].value < 0xffff))
        ++i;
    if ((hid_usage_page_strings[i].value != u_page) || (hid_usage_page_strings[i].value == 0xffff))
        return NULL;
    else
        return &hid_usage_page_strings[i];
}

/**
 * @brief 从指定的upage获取指定类型的usage type结构体。当不存在时，返回NULL
 *
 * @param upage 指定的upage
 * @param type usage的类型
 * @return const struct hid_usage_types_string * 目标usage type结构体。
 */
static __always_inline const struct hid_usage_types_string *hid_get_usage_type(
    const struct hid_usage_pages_string *upage, const int type)
{
    if (unlikely(upage == NULL || upage->types == NULL))
    {
        BUG_ON(1);
        return NULL;
    }
    struct hid_usage_types_string *types = upage->types;
    int i = 0;
    while ((types[i].value < type) && (types[i].value != 0xffff))
        ++i;

    if ((types[i].value != type) || (types[i].value == 0xffff))
        return NULL;

    return &types[i];
}

/**
 * @brief 获取usage page的名称
 *
 * @param u_page usage page的id
 * @return const char* usage page的字符串
 */
static const char *hid_get_usage_page_str(const int u_page)
{

    const struct hid_usage_pages_string *upage = hid_get_usage_page(u_page);
    if (unlikely(upage == NULL))
    {
        sprintk(__tmp_usage_page_str, "Unknown Usage Page: %#04x", u_page);
        return __tmp_usage_page_str;
    }
    return upage->string;
}

/**
 * @brief 打印usage page的指定类型的usage
 *
 * @param page usage page id
 * @param type usage的类型
 * @return const char*
 */
static const char *hid_get_usage_type_str(const int page, const int type)
{
    const struct hid_usage_pages_string *upage = hid_get_usage_page(page);
    if (unlikely(upage == NULL))
    {
        sprintk(__tmp_usage_page_str, "Unknown Usage Page: %#04x", page);
        return __tmp_usage_page_str;
    }

    // button press, ordinal, or UTC
    if (page == 0x0009)
    {
        sprintk(__tmp_usage_page_str, "Button number %i", type);
        return __tmp_usage_page_str;
    }
    else if (page == 0x000a)
    {
        sprintk(__tmp_usage_page_str, "Ordinal %i", type);
        return __tmp_usage_page_str;
    }
    else if (page == 0x0010)
    {
        sprintk(__tmp_usage_page_str, "UTC %#04X", type);
        return __tmp_usage_page_str;
    }

    const struct hid_usage_types_string *usage_type = hid_get_usage_type(upage, type);
    if (unlikely(usage_type == NULL))
    {
        sprintk(__tmp_usage_page_str, "Usage Page %s, with Unknown Type: %#04X", upage->string, type);
        return __tmp_usage_page_str;
    }

    return usage_type->string;
}

/**
 * @brief 输出colection字符串
 *
 * @param value collection的值
 * @return const char*
 */
static const char *hid_get_collection_str(const int value)
{
    if (value <= 0x06)
        return hid_collection_str[value];
    else if (value <= 0x7f)
        return "Reserved";
    else if (value <= 0xff)
        return "Vendor-defined";
    else
        return "Error in get_collection_str(): value > 0xff";
}

/**
 * @brief 从parser的offset table中，根据report_id和report_type，获取表中指向offset字段的指针
 *
 * @param parser 解析器
 * @param report_id report_id
 * @param report_type report类型
 * @return int* 指向offset字段的指针
 */
static int *__get_report_offset(struct hid_parser *parser, const uint8_t report_id, const uint8_t report_type)
{
    int pos = 0;
    // 尝试从已有的report中获取
    while ((pos < HID_MAX_REPORT) && (parser->offset_table[pos][0] != 0)) // 当offset的id不为0时
    {
        if ((parser->offset_table[pos][0] == report_id) && (parser->offset_table[pos][1] == report_type))
            return &parser->offset_table[pos][2];
        ++pos;
    }
    // 在offset table中占用一个新的表项来存储这个report的offset
    if (pos < HID_MAX_REPORT)
    {
        ++parser->cnt_report;
        parser->offset_table[pos][0] = report_id;
        parser->offset_table[pos][1] = report_type;
        parser->offset_table[pos][2] = 0;
        return &parser->offset_table[pos][2];
    }
    // 当offset table满了，且未找到结果的时候，返回NULL
    return NULL;
}

static __always_inline bool __find_object(struct hid_parser *parser, struct hid_data_t *data)
{
    kdebug("target_type=%d report_id=%d, offset=%d, size=%d", data->type, data->report_id, data->offset, data->size);
    struct hid_data_t found_data = {0};

    while (hid_parse(parser, &found_data))
    {
        kdebug("size=%d, type=%d, report_id=%d, u_page=%d, usage=%d", found_data.size, found_data.type,
               found_data.report_id, found_data.path.node[0].u_page, found_data.path.node[0].usage);
        // 按照路径完整匹配data
        if ((data->path.size > 0) && (found_data.type == data->type) &&
            (memcmp(found_data.path.node, data->path.node, data->path.size * sizeof(struct hid_node_t)) == 0))
        {
            goto found;
        }
        // 通过report id以及offset匹配成功
        else if ((found_data.report_id == data->report_id) && (found_data.type == data->type) &&
                 (found_data.offset == data->offset))
        {
            goto found;
        }
    }
    return false;

found:;
    memcpy(data, &found_data, sizeof(struct hid_data_t));
    data->report_count = parser->report_count;
    return true;
}
/**
 * @brief 在hid report中寻找参数data给定的节点数据，并将结果写入到data中
 *
 * @param hid_report hid report 数据
 * @param report_size report_data的大小（字节）
 * @param data 要寻找的节点数据。
 * @return true 找到指定的节点
 * @return false 未找到指定的节点
 */
bool hid_parse_find_object(const void *hid_report, const int report_size, struct hid_data_t *data)
{
    struct hid_parser parser = {0};
    hid_reset_parser(&parser);
    parser.report_desc = hid_report;
    parser.report_desc_size = report_size;
    // HID_PARSE_OUTPUT = false;

    printk("\nFinding Coordinate value:");
    if (__find_object(&parser, data))
    {
        printk("    size: %i (in bits)\n"
               "  offset: %i (in bits)\n"
               "     min: %i\n"
               "     max: %i\n"
               "  attrib: 0x%02X (input, output, or feature, etc.)\n",
               data->size, data->offset, data->logical_min, data->logical_max, data->attribute);
        return true;
    }
    else
    {
        printk("  Did not find Coordinate value.\n");
        return false;
    }
}