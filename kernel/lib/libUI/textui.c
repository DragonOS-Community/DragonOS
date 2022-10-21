#include "textui.h"

#include "driver/uart/uart.h"
#include "screen_manager.h"
#include <common/atomic.h>
#include <common/errno.h>
#include <common/printk.h>
#include <common/string.h>

struct scm_ui_framework_t textui_framework;
static spinlock_t __window_id_lock = {1};
static uint32_t __window_max_id = 0;

// 暂时初始化16080个初始字符对象以及67个虚拟行对象
#define INITIAL_CHARS 16080
#define INITIAL_VLINES (int)(1080 / 16)
static struct textui_char_chromatic_t __initial_chars[INITIAL_CHARS] = {0};
static struct textui_vline_chromatic_t __initial_vlines[INITIAL_VLINES] = {0};
static struct textui_window_t __initial_window = {0}; // 初始窗口
static struct textui_private_info_t __private_info = {0};
static struct List __windows_list;
static spinlock_t change_lock;

/**
 * @brief 初始化window对象
 *
 * @param window 窗口对象
 * @param flags 标志位
 * @param vlines_num 虚拟行的总数
 * @param vlines_ptr 虚拟行数组指针
 * @param cperline 每行最大的字符数
 */
static int __textui_init_window(struct textui_window_t *window, uint8_t flags, uint16_t vlines_num, void *vlines_ptr,
                                uint16_t cperline)
{
    memset((window), 0, sizeof(struct textui_window_t));
    list_init(&(window)->list);
    window->lock.lock = 1;
    spin_lock(&__window_id_lock);
    window->id = __window_max_id++;
    spin_unlock(&__window_id_lock);
    window->flags = flags;
    window->vlines_num = vlines_num;
    window->vlines_used = 1;
    window->top_vline = 0;
    window->vline_operating = 0;
    window->chars_per_line = cperline;
    if (textui_is_chromatic(flags))
        window->vlines.chromatic = vlines_ptr;
    else
        window->vlines.normal = vlines_ptr;
    list_add(&__windows_list, &(window)->list);
}

/**
 * @brief 初始化虚拟行对象
 *
 * @param vline 虚拟行对象指针
 * @param chars_ptr 字符对象数组指针
 */
#define __textui_init_vline(vline, chars_ptr)                                                                          \
    do                                                                                                                 \
    {                                                                                                                  \
        memset(vline, 0, sizeof(struct textui_vline_chromatic_t));                                                     \
        (vline)->index = 0;                                                                                            \
        (vline)->chars = chars_ptr;                                                                                    \
    } while (0)

int textui_install_handler(struct scm_buffer_info_t *buf)
{
    // return printk_init(buf);
    uart_send_str(COM1, "textui_install_handler");
    return 0;
}

int textui_uninstall_handler(void *args)
{
    return 0;
}

int textui_enable_handler(void *args)
{
    uart_send_str(COM1, "textui_enable_handler\n");
    return 0;
}

int textui_disable_handler(void *args)
{
    return 0;
}

int textui_change_handler(struct scm_buffer_info_t *buf)
{
    memcpy((void *)buf->vaddr, (void *)(textui_framework.buf->vaddr), textui_framework.buf->size);
    textui_framework.buf = buf;
    
    return 0;
}

struct scm_ui_framework_operations_t textui_ops = {
    .install = &textui_install_handler,
    .uninstall = &textui_uninstall_handler,
    .change = &textui_change_handler,
    .enable = &textui_enable_handler,
    .disable = &textui_disable_handler,
};

/**
 * @brief 获取textui的帧缓冲区能容纳的内容的行数
 *
 * @return uint16_t
 */
uint16_t __textui_get_actual_lines()
{
    return __private_info.actual_line;
}

/**
 * @brief 获取当前渲染的窗口的id
 *
 * @return uint16_t
 */
uint32_t __textui_get_current_window_id()
{
    return __private_info.current_window->id;
}

/**
 * @brief 插入换行
 *
 * @param window 窗口结构体
 * @param vline_id 虚拟行号
 * @return int
 */
static int __textui_new_line(struct textui_window_t *window, uint16_t vline_id)
{
    // todo: 支持在两个虚拟行之间插入一个新行

    ++window->vline_operating;

    if (unlikely(window->vline_operating == window->vlines_num))
        window->vline_operating = 0;
    struct textui_vline_chromatic_t *vline = &window->vlines.chromatic[window->vline_operating];
    memset(vline->chars, 0, sizeof(struct textui_char_chromatic_t) * window->chars_per_line);
    vline->index = 0;

    if (likely(window->vlines_used == window->vlines_num)) // 需要滚动屏幕
    {

        ++window->top_vline;

        if (unlikely(window->top_vline >= window->vlines_num))
            window->top_vline = 0;

        // 刷新所有行
        textui_refresh_vlines(window, window->top_vline, window->vlines_num);
    }
    else
        ++window->vlines_used;

    return 0;
}

/**
 * @brief 真正向屏幕上输出字符的函数
 *
 * @param window
 * @param character
 * @return int
 */
static int __textui_putchar_window(struct textui_window_t *window, uint16_t character, uint32_t FRcolor,
                                   uint32_t BKcolor)
{
    if (textui_is_chromatic(window->flags)) // 启用彩色字符
    {
        struct textui_vline_chromatic_t *vline = &window->vlines.chromatic[window->vline_operating];

        vline->chars[vline->index].c = character;
        vline->chars[vline->index].FRcolor = FRcolor & 0xffffff;
        vline->chars[vline->index].BKcolor = BKcolor & 0xffffff;
        ++vline->index;
        textui_refresh_characters(window, window->vline_operating, vline->index - 1, 1);
        // 换行
        // 加入光标后，因为会识别光标，所以需超过该行最大字符数才能创建新行
        if (vline->index > window->chars_per_line)
        {
            __textui_new_line(window, window->vline_operating);
        }
    }
    else
    {
        // todo: 支持纯文本字符
        while (1)
            pause();
    }
    return 0;
}

/**
 * @brief 在指定窗口上输出一个字符
 *
 * @param window 窗口
 * @param character 字符
 * @param FRcolor 前景色（RGB）
 * @param BKcolor 背景色（RGB）
 * @return int
 */
int textui_putchar_window(struct textui_window_t *window, uint16_t character, uint32_t FRcolor, uint32_t BKcolor)
{
    if (unlikely(character == '\0'))
        return 0;
    if (!textui_is_chromatic(window->flags)) // 暂不支持纯文本窗口
        return 0;

    // uint64_t rflags = 0; // 加锁后rflags存储到这里
    spin_lock(&window->lock);
    uart_send(COM1, character);
    if (unlikely(character == '\n'))
    {
        // 换行时还需要输出\r
        uart_send(COM1, '\r');
        __textui_new_line(window, window->vline_operating);
        // spin_unlock_irqrestore(&window->lock, rflags);
        spin_unlock(&window->lock);
        return 0;
    }
    else if (character == '\t') // 输出制表符
    {
        int space_to_print = 8 - window->vlines.chromatic[window->vline_operating].index % 8;

        while (space_to_print--)
        {
            __textui_putchar_window(window, ' ', FRcolor, BKcolor);
        }
    }
    else if (character == '\b') // 退格
    {
        char bufff[128] = {0};
        --(window->vlines.chromatic[window->vline_operating].index);
        {
            uint16_t tmp = window->vlines.chromatic[window->vline_operating].index;
            if (tmp >= 0)
            {
                window->vlines.chromatic[window->vline_operating].chars[tmp].c = ' ';
                window->vlines.chromatic[window->vline_operating].chars[tmp].BKcolor = BKcolor & 0xffffff;
                textui_refresh_characters(window, window->vline_operating, tmp, 1);
            }
        }
        // 需要向上缩一行
        if (window->vlines.chromatic[window->vline_operating].index <= 0)
        {
            window->vlines.chromatic[window->vline_operating].index = 0;
            memset(window->vlines.chromatic[window->vline_operating].chars, 0,
                   sizeof(struct textui_char_chromatic_t) * window->chars_per_line);
            --(window->vline_operating);
            if (unlikely(window->vline_operating < 0))
                window->vline_operating = window->vlines_num - 1;

            // 考虑是否向上滚动
            if (likely(window->vlines_used > __private_info.actual_line))
            {
                --window->top_vline;
                if (unlikely(window->top_vline < 0))
                    window->top_vline = window->vlines_num - 1;
            }
            --window->vlines_used;
            textui_refresh_vlines(window, window->top_vline, __private_info.actual_line);
        }
    }
    else
    {
        if (window->vlines.chromatic[window->vline_operating].index == window->chars_per_line)
            __textui_new_line(window, window->vline_operating);
        __textui_putchar_window(window, character, FRcolor, BKcolor);
    }

    // spin_unlock_irqrestore(&window->lock, rflags);
    spin_unlock(&window->lock);
    return 0;
}

/**
 * @brief 在默认窗口上输出一个字符
 *
 * @param character 字符
 * @param FRcolor 前景色（RGB）
 * @param BKcolor 背景色（RGB）
 * @return int
 */
int textui_putchar(uint16_t character, uint32_t FRcolor, uint32_t BKcolor)
{

    return textui_putchar_window(__private_info.default_window, character, FRcolor, BKcolor);
}

/**
 * @brief 初始化text ui框架
 *
 * @return int 
 */
int textui_init()
{
    spin_init(&change_lock);

    spin_init(&__window_id_lock);
    __window_max_id = 0;
    list_init(&__windows_list);
    memset(&textui_framework, 0, sizeof(struct scm_ui_framework_t));
    memset(&__private_info, 0, sizeof(struct textui_private_info_t));

    io_mfence();
    char name[] = "textUI";
    strcpy(textui_framework.name, name);

    textui_framework.ui_ops = &textui_ops;
    textui_framework.type = 0;

    // 注册框架到屏幕管理器
    int retval = scm_register(&textui_framework);
    if (retval != 0)
    {
        uart_send_str(COM1, "text ui init failed\n");
        while (1)
            pause();
    }

    uint16_t chars_per_vline = textui_framework.buf->width / TEXTUI_CHAR_WIDTH;
    uint16_t total_vlines = textui_framework.buf->height / TEXTUI_CHAR_HEIGHT;
    int cnt = chars_per_vline * total_vlines;

    struct textui_vline_chromatic_t *vl_ptr = __initial_vlines;
    struct textui_char_chromatic_t *ch_ptr = __initial_chars;

    // 初始化虚拟行
    for (int i = 0; i < total_vlines; ++i)
    {
        __textui_init_vline((vl_ptr + i), (ch_ptr + i * chars_per_vline));
    }

    // 初始化窗口
    __textui_init_window((&__initial_window), TEXTUI_WF_CHROMATIC, total_vlines, __initial_vlines, chars_per_vline);
    __private_info.current_window = &__initial_window;
    __private_info.default_window = &__initial_window;
    __private_info.actual_line = textui_framework.buf->height / TEXTUI_CHAR_HEIGHT;

    uart_send_str(COM1, "text ui initialized\n");
    return 0;
}
