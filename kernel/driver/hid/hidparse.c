#include <common/compiler.h>
#include <common/glib.h>
#include <common/hid.h>
#include <common/printk.h>
#include <common/string.h>
#include <debug/bug.h>

/*
    参考文档：https://www.usb.org/document-library/device-class-definition-hid-111
 */

static bool HID_PARSE_OUTPUT = true; // 是否输出解析信息

static void hid_reset_parser(struct hid_parser *parser);

static const char *hid_print_usage_page(const int u_page);
static const char *hid_print_usage_type(const int page, const int type);
static const char *hid_print_collection(const int value);
static int *__get_report_offset(struct hid_parser *hid_parser, const uint8_t report_id, const uint8_t report_type);

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

// 这部分请参考hid_1_11.pdf Section 6.2.2.4

#define HID_ITEM_COLLECTION 0xA0
#define HID_ITEM_END_COLLECTION 0xC0
#define HID_ITEM_FEATURE 0xB0
#define HID_ITEM_INPUT 0x80
#define HID_ITEM_OUTPUT 0x90

static char __spaces_buf[33];
char *__spaces(uint8_t cnt)
{
    static char __space_overflow_str[] = "**";
    if (cnt > 32)
    {
        return &__space_overflow_str;
    }

    memset(__spaces_buf, ' ', 32);
    __spaces_buf[cnt] = '\0';
    return __spaces_buf;
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
                printk("%sUsage Page (%s)", __spaces(space_cnt), hid_print_usage_page(parser->u_page));
            // 拷贝到 usage table。由于这是一个USAGE entry，因此不增加usage_size(以便后面覆盖它)
            parser->usage_table[parser->usage_size].u_page = parser->u_page;
            parser->usage_table[parser->usage_size].usage = 0xff;
            break;
        case HID_ITEM_USAGE:
            // 拷贝upage到usage table中
            if (parser->item & HID_SIZE_MASK > 2) // item大小为32字节
                parser->usage_table[parser->usage_size].u_page = (int)(parser->value >> 16);
            else
                parser->usage_table[parser->usage_size].u_page = parser->u_page;

            if (HID_PARSE_OUTPUT)
                printk("%sUsage (%s)", __spaces(space_cnt),
                       hid_print_usage_type(parser->u_page, parser->value & 0xffff));
            ++parser->usage_size;
            break;
        case HID_ITEM_USAGE_MIN:
            // todo: 设置usage min
            if (HID_PARSE_OUTPUT)
                printk("%sUsage min (%i=%s)", __spaces(space_cnt), parser->value,
                       hid_print_usage_type(parser->u_page, parser->value));
            break;
        case HID_ITEM_USAGE_MAX:
            // todo: 设置usage max
            if (HID_PARSE_OUTPUT)
                printk("%sUsage max (%i=%s)", __spaces(space_cnt), parser->value,
                       hid_print_usage_type(parser->u_page, parser->value));
            break;
        case HID_ITEM_COLLECTION:
            // 从usage table中取出第一个u_page和usage，并且将他们存储在parser->data.path
            parser->data.path.node[parser->data.path.size].u_page = parser->usage_table[0].u_page;
            parser->data.path.node[parser->data.path.size].usage = parser->usage_table[0].usage;
            ++parser->data.path.size;

            // 由于上面取出了元素，因此将队列往前移动1个位置
            __pop_usage_stack(parser);

            // 获取index(如果有的话)???
            if (parser->value > 0x80)
            {
                kdebug("parser->value > 0x80");
                parser->data.path.node[parser->data.path.size].u_page = 0xff;
                parser->data.path.node[parser->data.path.size].usage = parser->value & 0x7f;
                ++parser->data.path.size;
            }
            if (HID_PARSE_OUTPUT)
            {
                printk("%sCollection (%s)", __spaces(space_cnt), hid_print_collection(parser->value));
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
            parser->data.attribue = (uint8_t)parser->value;
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
            *offset_ptr = (*offset_ptr) + 1;

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
        case HID_ITEM_REP_ID:
            // todo:
            break;
        case HID_ITEM_REP_SIZE:
            // todo:
            break;
        case HID_ITEM_REP_COUNT:
            // todo:
            break;
        case HID_ITEM_UNIT_EXP:
            // todo:
            break;
        case HID_ITEM_UNIT:
            // todo:
            break;
        }
    }
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
 * @brief 打印usage page的数据
 *
 * @param u_page usage page的id
 * @return const char* usage page的字符串
 */
static const char *hid_print_usage_page(const int u_page)
{
    // todo:
    return NULL;
}

/**
 * @brief 打印usage page的类型
 *
 * @param page
 * @param type
 * @return const char*
 */
static const char *hid_print_usage_type(const int page, const int type)
{
    // todo:
    return NULL;
}

/**
 * @brief 输出colection字符串
 *
 * @param value
 * @return const char*
 */
static const char *hid_print_collection(const int value)
{
    // todo:
    return NULL;
}

/**
 * @brief 从parser的offset table中，根据report_id和report_type，获取表中指向offset字段的指针
 *
 * @param hid_parser 解析器
 * @param report_id report_id
 * @param report_type report类型
 * @return int* 指向offset字段的指针
 */
static int *__get_report_offset(struct hid_parser *hid_parser, const uint8_t report_id, const uint8_t report_type)
{
    // todo:
}