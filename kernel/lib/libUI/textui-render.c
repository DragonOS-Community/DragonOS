#include "textui.h"
#include <driver/uart/uart.h>
#include <common/errno.h>
#include "screen_manager.h"

#define WHITE 0x00ffffff  //白
#define BLACK 0x00000000  //黑
#define RED 0x00ff0000    //红
#define ORANGE 0x00ff8000 //橙
#define YELLOW 0x00ffff00 //黄
#define GREEN 0x0000ff00  //绿
#define BLUE 0x000000ff   //蓝
#define INDIGO 0x0000ffff //靛
#define PURPLE 0x008000ff //紫

// 根据rgb计算出最终的颜色值
#define calculate_color(r, g, b) ((((r & 0xff) << 16) | ((g & 0xff) << 8) | (b & 0xff)) & 0x00ffffff)

extern struct scm_ui_framework_t textui_framework;

extern unsigned char font_ascii[256][16]; //导出ascii字体的bitmap（8*16大小） ps:位于font.h中
static void __textui_render_chromatic(uint16_t actual_line, uint16_t index, struct textui_char_chromatic_t *character);

/**
 * @brief 重新渲染整个虚拟行
 *
 * @param window 窗口结构体
 * @param vline_id 虚拟行号
 * @return int 错误码
 */
int textui_refresh_vline(struct textui_window_t *window, uint16_t vline_id)
{
    if (textui_is_chromatic(window->flags))
        return textui_refresh_characters(window, vline_id, 0, window->chars_per_line);
    else
        return textui_refresh_characters(window, vline_id, 0, window->chars_per_line);
}

int textui_refresh_vlines(struct textui_window_t *window, uint16_t start, uint16_t count)
{
    char bufff[16] = {0};
    // uart_send_str(COM1, "  BEGIN  ");
    for (int i = start; i < window->vlines_num && count > 0; ++i, --count)
    {
        // sprintk(bufff, "[ 1fresh: %d ] ", i);
        // uart_send_str(COM1, bufff);
        textui_refresh_vline(window, i);
    }
    start = 0;
    while (count > 0)
    {
        // sprintk(bufff, "[ 2fresh: %d ] ", start);
        // uart_send_str(COM1, bufff);
        // sprintk(bufff, " index=%d ", (window->vlines.chromatic)[start].index);
        // uart_send_str(COM1, bufff);
        textui_refresh_vline(window, start);
        ++start;
        --count;
    }
    // uart_send_str(COM1, "  END  ");
    return 0;
}

/**
 * @brief 刷新某个虚拟行的连续n个字符对象
 *
 * @param window 窗口结构体
 * @param vline_id 虚拟行号
 * @param start 起始字符号
 * @param count 要刷新的字符数量
 * @return int 错误码
 */
int textui_refresh_characters(struct textui_window_t *window, uint16_t vline_id, uint16_t start, uint16_t count)
{
    if (window->id != __textui_get_current_window_id())
        return 0;
    // 判断虚拟行参数是否合法
    if (unlikely(vline_id >= window->vlines_num && (start + count) > window->chars_per_line))
        return -EINVAL;

    // 计算虚拟行对应的真实行
    int actual_line_id = (int)vline_id - window->top_vline;
    if (actual_line_id < 0)
        actual_line_id += __textui_get_actual_lines();
    // 判断真实行id是否合理
    if (unlikely(actual_line_id < 0 || actual_line_id >= __textui_get_actual_lines()))
        return 0;

    // 若是彩色像素模式
    if (textui_is_chromatic(window->flags))
    {
        struct textui_vline_chromatic_t *vline = &(window->vlines.chromatic)[vline_id];
        for (int i = 0; i < count; ++i)
        {

            __textui_render_chromatic(actual_line_id, start + i, &vline->chars[start + i]);
        }
    }

    return 0;
}

/**
 * @brief 渲染彩色字符
 *
 * @param actual_line 真实行的行号
 * @param index 列号
 * @param character 要渲染的字符
 */
static void __textui_render_chromatic(uint16_t actual_line, uint16_t index, struct textui_char_chromatic_t *character)
{
    /**
     * @brief 在屏幕上指定位置打印字符
     *
     * @param x 左上角列像素点位置
     * @param y 左上角行像素点位置
     * @param FRcolor 字体颜色
     * @param BKcolor 背景颜色
     * @param font 字符的bitmap
     */

    unsigned char *font_ptr = font_ascii[(uint8_t)character->c];
    unsigned int *addr;
    uint32_t *fb = (uint32_t *)textui_framework.buf->vaddr;

    uint32_t FRcolor = character->FRcolor & 0x00ffffff;

    uint32_t BKcolor = character->BKcolor & 0x00ffffff;

    uint32_t x = index * TEXTUI_CHAR_WIDTH;
    uint32_t y = actual_line * TEXTUI_CHAR_HEIGHT;

    int testbit; // 用来测试某位是背景还是字体本身

    for (int i = 0; i < TEXTUI_CHAR_HEIGHT; ++i)
    {
        // 计算出帧缓冲区的地址
        addr = (uint32_t *)(fb + textui_framework.buf->width * (y + i) + x);

        testbit = (1 << (TEXTUI_CHAR_WIDTH + 1));
        for (int j = 0; j < TEXTUI_CHAR_WIDTH; ++j)
        {
            // 从左往右逐个测试相应位
            testbit >>= 1;
            if (*font_ptr & testbit)
                *addr = FRcolor; // 字，显示前景色
            else
                *addr = BKcolor; // 背景色

            ++addr;
        }
        ++font_ptr;
    }
}