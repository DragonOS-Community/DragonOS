//
// Created by longjin on 2022/1/22.
//
#include "printk.h"
#include "kprint.h"
#include <driver/multiboot2/multiboot2.h>
#include <mm/mm.h>
#include <common/spinlock.h>

#include <driver/uart/uart.h>
#include <driver/video/video.h>
#include "math.h"
#include <common/string.h>

struct printk_screen_info pos;
extern ul VBE_FB_phys_addr; // 由bootloader传来的帧缓存区的物理地址
static spinlock_t printk_lock;
static bool sw_show_scroll_animation = false; // 显示换行动画的开关

/**
 * @brief Set the printk pos object
 *
 * @param x 列坐标
 * @param y 行坐标
 */
static int set_printk_pos(const int x, const int y);

/**
 * @brief 在屏幕上指定位置打印字符
 *
 * @param fb 帧缓存线性地址
 * @param Xsize 行分辨率
 * @param x 左上角列像素点位置
 * @param y 左上角行像素点位置
 * @param FRcolor 字体颜色
 * @param BKcolor 背景颜色
 * @param font 字符的bitmap
 */
static void putchar(uint *fb, int Xsize, int x, int y, unsigned int FRcolor, unsigned int BKcolor, unsigned char font);

static uint *get_pos_VBE_FB_addr();

/**
 * @brief 清屏
 *
 */
static int cls();

#pragma GCC push_options
#pragma GCC optimize("O0")
/**
 * @brief 滚动窗口（尚不支持向下滚动)
 *
 * @param direction  方向，向上滑动为true,否则为false
 * @param pixels 要滑动的像素数量
 * @param animation 是否包含滑动动画
 */
static int scroll(bool direction, int pixels, bool animation);
#pragma GCC pop_options
/**
 * @brief 将数字按照指定的要求转换成对应的字符串（2~36进制）
 *
 * @param str 要返回的字符串
 * @param num 要打印的数值
 * @param base 基数
 * @param field_width 区域宽度
 * @param precision 精度
 * @param flags 标志位
 */
static char *write_num(char *str, ul num, int base, int field_width, int precision, int flags);

static char *write_float_point_num(char *str, double num, int field_width, int precision, int flags);

static int calculate_max_charNum(int len, int size)
{
    /**
     * @brief 计算屏幕上能有多少行
     * @param len 屏幕长/宽
     * @param size 字符长/宽
     */
    return len / size - 1;
}

int printk_init(const int char_size_x, const int char_size_y)
{
    struct multiboot_tag_framebuffer_info_t info;
    int reserved;

    multiboot2_iter(multiboot2_get_Framebuffer_info, &info, &reserved);

    pos.width = info.framebuffer_width;
    pos.height = info.framebuffer_height;

    pos.char_size_x = char_size_x;
    pos.char_size_y = char_size_y;
    pos.max_x = calculate_max_charNum(pos.width, char_size_x);
    pos.max_y = calculate_max_charNum(pos.height, char_size_y);

    VBE_FB_phys_addr = (ul)info.framebuffer_addr;

    pos.FB_address = (uint *)0xffff800003000000;
    pos.FB_length = 1UL * pos.width * pos.height;

    // 初始化自旋锁
    spin_init(&printk_lock);

    // ======== 临时的将物理地址填写到0x0000000003000000处 之后会在mm内将帧缓存区重新映射=====

    ul global_CR3 = (ul)get_CR3();
    ul fb_virt_addr = (ul)pos.FB_address;
    ul fb_phys_addr = VBE_FB_phys_addr;

    // 计算帧缓冲区的线性地址对应的pml4页表项的地址
    ul *tmp = phys_2_virt((ul *)((ul)global_CR3 & (~0xfffUL)) + ((fb_virt_addr >> PAGE_GDT_SHIFT) & 0x1ff));

    tmp = phys_2_virt((ul *)(*tmp & (~0xfffUL)) + ((fb_virt_addr >> PAGE_1G_SHIFT) & 0x1ff));

    ul *tmp1;
    // 初始化2M物理页
    for (ul i = 0; i < (pos.FB_length << 2); i += PAGE_2M_SIZE)
    {
        // 计算当前2M物理页对应的pdt的页表项的物理地址
        tmp1 = phys_2_virt((ul *)(*tmp & (~0xfffUL)) + (((fb_virt_addr + i) >> PAGE_2M_SHIFT) & 0x1ff));

        // 页面写穿，禁止缓存
        set_pdt(tmp1, mk_pdt((ul)fb_phys_addr + i, PAGE_KERNEL_PAGE | PAGE_PWT | PAGE_PCD));
    }

    flush_tlb();

    pos.x = 0;
    pos.y = 0;

    cls();

    kdebug("width=%d\theight=%d", pos.width, pos.height);
    // 由于此时系统并未启用双缓冲，因此关闭滚动动画
    printk_disable_animation();
    return 0;
}

static int set_printk_pos(const int x, const int y)
{
    // 指定的坐标不在屏幕范围内
    if (!((x >= 0 && x <= pos.max_x) && (y >= 0 && y <= pos.max_y)))
        return EPOS_OVERFLOW;
    pos.x = x;
    pos.y = y;
    return 0;
}
static int skip_and_atoi(const char **s)
{
    /**
     * @brief 获取连续的一段字符对应整数的值
     * @param:**s 指向 指向字符串的指针 的指针
     */
    int ans = 0;
    while (is_digit(**s))
    {
        ans = ans * 10 + (**s) - '0';
        ++(*s);
    }
    return ans;
}

static void auto_newline()
{
    /**
     * @brief 超过每行最大字符数，自动换行
     *
     */

    if (pos.x > pos.max_x)
    {
#ifdef DEBUG
        uart_send(COM1, '\r');
        uart_send(COM1, '\n');
#endif
        pos.x = 0;
        ++pos.y;
    }
    if (pos.y > pos.max_y)
    {
#ifdef DEBUG
        uart_send(COM1, '\r');
        uart_send(COM1, '\n');
#endif
        pos.y = pos.max_y;
        int lines_to_scroll = 1;
        barrier();
        scroll(true, lines_to_scroll * pos.char_size_y, sw_show_scroll_animation);
        barrier();
        pos.y -= (lines_to_scroll - 1);
    }
}

int vsprintf(char *buf, const char *fmt, va_list args)
{
    /**
     * 将字符串按照fmt和args中的内容进行格式化，然后保存到buf中
     * @param buf 结果缓冲区
     * @param fmt 格式化字符串
     * @param args 内容
     * @return 最终字符串的长度
     */

    char *str, *s;

    str = buf;

    int flags;       // 用来存储格式信息的bitmap
    int field_width; //区域宽度
    int precision;   //精度
    int qualifier;   //数据显示的类型
    int len;

    //开始解析字符串
    for (; *fmt; ++fmt)
    {
        //内容不涉及到格式化，直接输出
        if (*fmt != '%')
        {
            *str = *fmt;
            ++str;
            continue;
        }

        //开始格式化字符串

        //清空标志位和field宽度
        field_width = flags = 0;

        bool flag_tmp = true;
        bool flag_break = false;

        ++fmt;
        while (flag_tmp)
        {
            switch (*fmt)
            {
            case '\0':
                //结束解析
                flag_break = true;
                flag_tmp = false;
                break;

            case '-':
                // 左对齐
                flags |= LEFT;
                ++fmt;
                break;
            case '+':
                //在正数前面显示加号
                flags |= PLUS;
                ++fmt;
                break;
            case ' ':
                flags |= SPACE;
                ++fmt;
                break;
            case '#':
                //在八进制数前面显示 '0o'，在十六进制数前面显示 '0x' 或 '0X'
                flags |= SPECIAL;
                ++fmt;
                break;
            case '0':
                //显示的数字之前填充‘0’来取代空格
                flags |= PAD_ZERO;
                ++fmt;
                break;
            default:
                flag_tmp = false;
                break;
            }
        }
        if (flag_break)
            break;

        //获取区域宽度
        field_width = -1;
        if (*fmt == '*')
        {
            field_width = va_arg(args, int);
            ++fmt;
        }
        else if (is_digit(*fmt))
        {
            field_width = skip_and_atoi(&fmt);
            if (field_width < 0)
            {
                field_width = -field_width;
                flags |= LEFT;
            }
        }

        //获取小数精度
        precision = -1;
        if (*fmt == '.')
        {
            ++fmt;
            if (*fmt == '*')
            {
                precision = va_arg(args, int);
                ++fmt;
            }
            else if is_digit (*fmt)
            {
                precision = skip_and_atoi(&fmt);
            }
        }

        //获取要显示的数据的类型
        if (*fmt == 'h' || *fmt == 'l' || *fmt == 'L' || *fmt == 'Z')
        {
            qualifier = *fmt;
            ++fmt;
        }
        //为了支持lld
        if (qualifier == 'l' && *fmt == 'l', *(fmt + 1) == 'd')
            ++fmt;

        //转化成字符串
        long long *ip;
        switch (*fmt)
        {
        //输出 %
        case '%':
            *str++ = '%';

            break;
        // 显示一个字符
        case 'c':
            //靠右对齐
            if (!(flags & LEFT))
            {
                while (--field_width > 0)
                {
                    *str = ' ';
                    ++str;
                }
            }

            *str++ = (unsigned char)va_arg(args, int);

            while (--field_width > 0)
            {
                *str = ' ';
                ++str;
            }

            break;

        //显示一个字符串
        case 's':
            s = va_arg(args, char *);
            if (!s)
                s = '\0';
            len = strlen(s);
            if (precision < 0)
            {
                //未指定精度
                precision = len;
            }

            else if (len > precision)
            {
                len = precision;
            }

            //靠右对齐
            if (!(flags & LEFT))
                while (len < field_width--)
                {
                    *str = ' ';
                    ++str;
                }

            for (int i = 0; i < len; i++)
            {
                *str = *s;
                ++s;
                ++str;
            }

            while (len < field_width--)
            {
                *str = ' ';
                ++str;
            }

            break;
        //以八进制显示字符串
        case 'o':
            flags |= SMALL;
        case 'O':
            flags |= SPECIAL;
            if (qualifier == 'l')
                str = write_num(str, va_arg(args, long long), 8, field_width, precision, flags);
            else
                str = write_num(str, va_arg(args, int), 8, field_width, precision, flags);
            break;

        //打印指针指向的地址
        case 'p':
            if (field_width == 0)
            {
                field_width = 2 * sizeof(void *);
                flags |= PAD_ZERO;
            }

            str = write_num(str, (unsigned long)va_arg(args, void *), 16, field_width, precision, flags);

            break;

        //打印十六进制
        case 'x':
            flags |= SMALL;
        case 'X':
            // flags |= SPECIAL;
            if (qualifier == 'l')
                str = write_num(str, va_arg(args, ll), 16, field_width, precision, flags);
            else
                str = write_num(str, va_arg(args, int), 16, field_width, precision, flags);
            break;

        //打印十进制有符号整数
        case 'i':
        case 'd':

            flags |= SIGN;
            if (qualifier == 'l')
                str = write_num(str, va_arg(args, long long), 10, field_width, precision, flags);
            else
                str = write_num(str, va_arg(args, int), 10, field_width, precision, flags);
            break;

        //打印十进制无符号整数
        case 'u':
            if (qualifier == 'l')
                str = write_num(str, va_arg(args, unsigned long long), 10, field_width, precision, flags);
            else
                str = write_num(str, va_arg(args, unsigned int), 10, field_width, precision, flags);
            break;

        //输出有效字符数量到*ip对应的变量
        case 'n':

            if (qualifier == 'l')
                ip = va_arg(args, long long *);
            else
                ip = (ll *)va_arg(args, int *);

            *ip = str - buf;
            break;
        case 'f':
            // 默认精度为3
            // printk("1111\n");
            // va_arg(args, double);
            // printk("222\n");

            if (precision < 0)
                precision = 3;

            str = write_float_point_num(str, va_arg(args, double), field_width, precision, flags);

            break;

        //对于不识别的控制符，直接输出
        default:
            *str++ = '%';
            if (*fmt)
                *str++ = *fmt;
            else
                --fmt;
            break;
        }
    }
    *str = '\0';

    //返回缓冲区已有字符串的长度。
    return str - buf;
}

static char *write_num(char *str, ul num, int base, int field_width, int precision, int flags)
{
    /**
     * @brief 将数字按照指定的要求转换成对应的字符串
     *
     * @param str 要返回的字符串
     * @param num 要打印的数值
     * @param base 基数
     * @param field_width 区域宽度
     * @param precision 精度
     * @param flags 标志位
     */

    // 首先判断是否支持该进制
    if (base < 2 || base > 36)
        return 0;
    char pad, sign, tmp_num[100];

    const char *digits = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    // 显示小写字母
    if (flags & SMALL)
        digits = "0123456789abcdefghijklmnopqrstuvwxyz";

    if (flags & LEFT)
        flags &= ~PAD_ZERO;
    // 设置填充元素
    pad = (flags & PAD_ZERO) ? '0' : ' ';

    sign = 0;

    if (flags & SIGN)
    {
        int64_t signed_num = (int64_t)num;
        if (signed_num < 0)
        {
            sign = '-';
            num = -signed_num;
        }
        else
            num = signed_num;
    }
    else
    {
        // 设置符号
        sign = (flags & PLUS) ? '+' : ((flags & SPACE) ? ' ' : 0);
    }

    // sign占用了一个宽度
    if (sign)
        --field_width;

    if (flags & SPECIAL)
        if (base == 16) // 0x占用2个位置
            field_width -= 2;
        else if (base == 8) // O占用一个位置
            --field_width;

    int js_num = 0; // 临时数字字符串tmp_num的长度

    if (num == 0)
        tmp_num[js_num++] = '0';
    else
    {
        num = ABS(num);
        //进制转换
        while (num > 0)
        {
            tmp_num[js_num++] = digits[num % base]; // 注意这里，输出的数字，是小端对齐的。低位存低位
            num /= base;
        }
    }

    if (js_num > precision)
        precision = js_num;

    field_width -= precision;

    // 靠右对齐
    if (!(flags & (LEFT + PAD_ZERO)))
        while (field_width-- > 0)
            *str++ = ' ';

    if (sign)
        *str++ = sign;
    if (flags & SPECIAL)
        if (base == 16)
        {
            *str++ = '0';
            *str++ = digits[33];
        }
        else if (base == 8)
            *str++ = digits[24]; //注意这里是英文字母O或者o
    if (!(flags & LEFT))
        while (field_width-- > 0)
            *str++ = pad;
    while (js_num < precision)
    {
        --precision;
        *str++ = '0';
    }

    while (js_num-- > 0)
        *str++ = tmp_num[js_num];

    while (field_width-- > 0)
        *str++ = ' ';

    return str;
}

static char *write_float_point_num(char *str, double num, int field_width, int precision, int flags)
{
    /**
     * @brief 将浮点数按照指定的要求转换成对应的字符串
     *
     * @param str 要返回的字符串
     * @param num 要打印的数值
     * @param field_width 区域宽度
     * @param precision 精度
     * @param flags 标志位
     */

    char pad, sign, tmp_num_z[100], tmp_num_d[350];

    const char *digits = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    // 显示小写字母
    if (flags & SMALL)
        digits = "0123456789abcdefghijklmnopqrstuvwxyz";

    // 设置填充元素
    pad = (flags & PAD_ZERO) ? '0' : ' ';
    sign = 0;
    if (flags & SIGN && num < 0)
    {
        sign = '-';
        num = -num;
    }
    else
    {
        // 设置符号
        sign = (flags & PLUS) ? '+' : ((flags & SPACE) ? ' ' : 0);
    }

    // sign占用了一个宽度
    if (sign)
        --field_width;

    int js_num_z = 0, js_num_d = 0;                                                     // 临时数字字符串tmp_num_z tmp_num_d的长度
    uint64_t num_z = (uint64_t)(num);                                                   // 获取整数部分
    uint64_t num_decimal = (uint64_t)(round(1.0 * (num - num_z) * pow(10, precision))); // 获取小数部分

    if (num == 0 || num_z == 0)
        tmp_num_z[js_num_z++] = '0';
    else
    {
        //存储整数部分
        while (num_z > 0)
        {
            tmp_num_z[js_num_z++] = digits[num_z % 10]; // 注意这里，输出的数字，是小端对齐的。低位存低位
            num_z /= 10;
        }
    }

    while (num_decimal > 0)
    {
        tmp_num_d[js_num_d++] = digits[num_decimal % 10];
        num_decimal /= 10;
    }

    field_width -= (precision + 1 + js_num_z);

    // 靠右对齐
    if (!(flags & LEFT))
        while (field_width-- > 0)
            *str++ = pad;

    if (sign)
        *str++ = sign;

    // 输出整数部分
    // while (js_num_z-- > 0)
    //     *str++ = tmp_num_z[js_num_z];
    while (js_num_z > 0)
    {
        *str++ = tmp_num_z[js_num_z - 1];
        --js_num_z;
    }
    *str++ = '.';

    // 输出小数部分
    int total_dec_count = js_num_d;
    for (int i = 0; i < precision && js_num_d-- > 0; ++i)
        *str++ = tmp_num_d[js_num_d];

    while (total_dec_count < precision)
    {
        ++total_dec_count;
        *str++ = '0';
    }

    while (field_width-- > 0)
        *str++ = ' ';

    return str;
}

static void putchar(uint *fb, int Xsize, int x, int y, unsigned int FRcolor, unsigned int BKcolor, unsigned char font)
{
    /**
     * @brief 在屏幕上指定位置打印字符
     *
     * @param fb 帧缓存线性地址
     * @param Xsize 行分辨率
     * @param x 左上角列像素点位置
     * @param y 左上角行像素点位置
     * @param FRcolor 字体颜色
     * @param BKcolor 背景颜色
     * @param font 字符的bitmap
     */

    //#if DEBUG
    uart_send(COM1, font);
    //#endif

    unsigned char *font_ptr = font_ascii[font];
    unsigned int *addr;

    int testbit; // 用来测试某位是背景还是字体本身

    for (int i = 0; i < pos.char_size_y; ++i)
    {
        // 计算出帧缓冲区的地址
        addr = fb + Xsize * (y + i) + x;
        testbit = (1 << (pos.char_size_x + 1));
        for (int j = 0; j < pos.char_size_x; ++j)
        {
            //从左往右逐个测试相应位
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

/**
 * @brief 格式化打印字符串
 *
 * @param FRcolor 前景色
 * @param BKcolor 背景色
 * @param ... 格式化字符串
 */
int printk_color(unsigned int FRcolor, unsigned int BKcolor, const char *fmt, ...)
{

    uint64_t rflags = 0; // 加锁后rflags存储到这里
    spin_lock_irqsave(&printk_lock, rflags);

    va_list args;
    va_start(args, fmt);
    char buf[4096]; // vsprintf()的缓冲区
    int len = vsprintf(buf, fmt, args);

    va_end(args);
    unsigned char current;

    int i; // 总共输出的字符数
    for (i = 0; i < len; ++i)
    {
        current = *(buf + i);
        //输出换行
        if (current == '\n')
        {
            pos.x = 0;
            ++pos.y;
            auto_newline();
        }
        else if (current == '\t') // 输出制表符
        {
            int space_to_print = 8 - pos.x % 8;

            while (space_to_print--)
            {
                putchar(pos.FB_address, pos.width, pos.x * pos.char_size_x, pos.y * pos.char_size_y, FRcolor, BKcolor, ' ');
                ++pos.x;

                auto_newline();
            }
        }
        else if (current == '\b') // 退格
        {
            --pos.x;
            if (pos.x < 0)
            {
                --pos.y;
                if (pos.y <= 0)
                    pos.x = pos.y = 0;
                else
                    pos.x = pos.max_x;
            }

            putchar(pos.FB_address, pos.width, pos.x * pos.char_size_x, pos.y * pos.char_size_y, FRcolor, BKcolor, ' ');

            auto_newline();
        }
        else
        {
            putchar(pos.FB_address, pos.width, pos.x * pos.char_size_x, pos.y * pos.char_size_y, FRcolor, BKcolor, current);
            ++pos.x;
            auto_newline();
        }
    }

    spin_unlock_irqrestore(&printk_lock, rflags);
    return i;
}

int do_scroll(bool direction, int pixels)
{
    if (direction == true) // 向上滚动
    {
        pixels = pixels;
        if (pixels > pos.height)
            return EPOS_OVERFLOW;
        // 无需滚动
        if (pixels == 0)
            return 0;
        unsigned int src = pixels * pos.width;
        unsigned int count = pos.FB_length - src;

        memcpy(pos.FB_address, (pos.FB_address + src), sizeof(unsigned int) * (pos.FB_length - src));
        memset(pos.FB_address + (pos.FB_length - src), 0, sizeof(unsigned int) * (src));

        return 0;
    }
    else
        return EUNSUPPORTED;
    return 0;
}
/**
 * @brief 滚动窗口（尚不支持向下滚动）
 *
 * @param direction  方向，向上滑动为true,否则为false
 * @param pixels 要滑动的像素数量
 * @param animation 是否包含滑动动画
 */
static int scroll(bool direction, int pixels, bool animation)
{
    // 暂时不支持反方向滚动
    if (direction == false)
        return EUNSUPPORTED;
    // 为了保证打印字符正确，需要对pixel按照字体高度对齐
    int md = pixels % pos.char_size_y;
    if (md)
        pixels = pixels + pos.char_size_y - md;
    if (animation == false)
        return do_scroll(direction, pixels);
    else
    {

        int steps;
        if (pixels > 10)
            steps = 5;
        else
            steps = pixels % 10;
        int half_steps = steps / 2;

        // 计算加速度
        double accelerate = 0.5 * pixels / (half_steps * half_steps);
        int current_pixels = 0;
        double delta_x;

        int trace[13] = {0};
        int js_trace = 0;
        // 加速阶段
        for (int i = 1; i <= half_steps; ++i)
        {
            trace[js_trace] = (int)(accelerate * i + 0.5);
            current_pixels += trace[js_trace];
            do_scroll(direction, trace[js_trace]);

            ++js_trace;
        }

        // 强制使得位置位于1/2*pixels
        if (current_pixels < pixels / 2)
        {
            delta_x = pixels / 2 - current_pixels;
            current_pixels += delta_x;
            do_scroll(direction, delta_x);
        }

        // 减速阶段，是加速阶段的重放
        for (int i = js_trace - 1; i >= 0; --i)
        {
            current_pixels += trace[i];
            do_scroll(direction, trace[i]);
        }

        if (current_pixels > pixels)
            kerror("During scrolling: scrolled pixels over bound!");

        // 强制使得位置位于pixels
        if (current_pixels < pixels)
        {
            delta_x = pixels - current_pixels;
            current_pixels += delta_x;
            do_scroll(direction, delta_x);
        }
    }

    return 0;
}

/**
 * @brief 清屏
 *
 */
static int cls()
{
    memset(pos.FB_address, BLACK, pos.FB_length * sizeof(unsigned int));
    pos.x = 0;
    pos.y = 0;
    return 0;
}

/**
 * @brief 获取VBE帧缓冲区长度
 */
ul get_VBE_FB_length()
{
    return pos.FB_length;
}

/**
 * @brief 设置pos变量中的VBE帧缓存区的线性地址
 * @param virt_addr VBE帧缓存区线性地址
 */
void set_pos_VBE_FB_addr(uint *virt_addr)
{
    pos.FB_address = (uint *)virt_addr;
}

static uint *get_pos_VBE_FB_addr()
{
    return pos.FB_address;
}

/**
 * @brief 使能滚动动画
 *
 */
void printk_enable_animation()
{
    sw_show_scroll_animation = true;
}
/**
 * @brief 禁用滚动动画
 *
 */
void printk_disable_animation()
{
    sw_show_scroll_animation = false;
}

int sprintk(char *buf, const char *fmt, ...)
{
    int count = 0;
    va_list args;

    va_start(args, fmt);
    count = vsprintf(buf, fmt, args);
    va_end(args);

    return count;
}

