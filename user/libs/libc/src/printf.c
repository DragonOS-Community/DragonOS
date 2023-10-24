#include <printf.h>

#include <libsystem/syscall.h>
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static char *write_num(char *str, uint64_t num, int base, int field_width, int precision, int flags);
static char *write_float_point_num(char *str, double num, int field_width, int precision, int flags);

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

/**
 * @brief 往屏幕上输出字符串
 *
 * @param str 字符串指针
 * @param front_color 前景色
 * @param bg_color 背景色
 * @return int64_t
 */
int64_t put_string(char *str, uint64_t front_color, uint64_t bg_color)
{
    return syscall_invoke(SYS_PUT_STRING, (uint64_t)str, front_color, bg_color, 0, 0, 0);
}

int printf(const char *fmt, ...)
{
    char buf[4096];
    int count = 0;
    va_list args;
    va_start(args, fmt);

    count = vsprintf(buf, fmt, args);
    va_end(args);
    // put_string(buf, COLOR_WHITE, COLOR_BLACK);
    write(1, buf, count);
    return count;
}

int sprintf(char *buf, const char *fmt, ...)
{
    int count = 0;
    va_list args;

    va_start(args, fmt);
    count = vsprintf(buf, fmt, args);
    va_end(args);

    return count;
}

/**
 * 将字符串按照fmt和args中的内容进行格式化，然后保存到buf中
 * @param buf 结果缓冲区
 * @param fmt 格式化字符串
 * @param args 内容
 * @return 最终字符串的长度
 */
int vsprintf(char *buf, const char *fmt, va_list args)
{
    // 当需要输出的字符串的指针为空时，使用该字符填充目标字符串的指针
    static const char __end_zero_char = '\0';

    char *str = NULL, *s = NULL;

    str = buf;

    int flags;       // 用来存储格式信息的bitmap
    int field_width; // 区域宽度
    int precision;   // 精度
    int qualifier;   // 数据显示的类型
    int len;

    // 开始解析字符串
    for (; *fmt; ++fmt)
    {
        // 内容不涉及到格式化，直接输出
        if (*fmt != '%')
        {
            *str = *fmt;
            ++str;
            continue;
        }

        // 开始格式化字符串

        // 清空标志位和field宽度
        field_width = flags = 0;

        bool flag_tmp = true;
        bool flag_break = false;

        ++fmt;
        while (flag_tmp)
        {
            switch (*fmt)
            {
            case '\0':
                // 结束解析
                flag_break = true;
                flag_tmp = false;
                break;

            case '-':
                // 左对齐
                flags |= LEFT;
                ++fmt;
                break;
            case '+':
                // 在正数前面显示加号
                flags |= PLUS;
                ++fmt;
                break;
            case ' ':
                flags |= SPACE;
                ++fmt;
                break;
            case '#':
                // 在八进制数前面显示 '0o'，在十六进制数前面显示 '0x' 或 '0X'
                flags |= SPECIAL;
                ++fmt;
                break;
            case '0':
                // 显示的数字之前填充‘0’来取代空格
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

        // 获取区域宽度
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

        // 获取小数精度
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

        // 获取要显示的数据的类型
        if (*fmt == 'h' || *fmt == 'l' || *fmt == 'L' || *fmt == 'Z')
        {
            qualifier = *fmt;
            ++fmt;
        }
        // 为了支持lld
        if (qualifier == 'l' && *fmt == 'l', *(fmt + 1) == 'd')
            ++fmt;

        // 转化成字符串
        long long *ip;
        switch (*fmt)
        {
        // 输出 %
        case '%':
            *str++ = '%';

            break;
        // 显示一个字符
        case 'c':
            // 靠右对齐
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

        // 显示一个字符串
        case 's':
            s = va_arg(args, char *);
            if (!s)
                s = &__end_zero_char;
            len = strlen(s);
            if (precision < 0)
            {
                // 未指定精度
                precision = len;
            }

            else if (len > precision)
            {
                len = precision;
            }

            // 靠右对齐
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
        // 以八进制显示字符串
        case 'o':
            flags |= SMALL;
        case 'O':
            flags |= SPECIAL;
            if (qualifier == 'l')
                str = write_num(str, va_arg(args, long long), 8, field_width, precision, flags);
            else
                str = write_num(str, va_arg(args, int), 8, field_width, precision, flags);
            break;

        // 打印指针指向的地址
        case 'p':
            if (field_width == 0)
            {
                field_width = 2 * sizeof(void *);
                flags |= PAD_ZERO;
            }

            str = write_num(str, (unsigned long)va_arg(args, void *), 16, field_width, precision, flags);

            break;

        // 打印十六进制
        case 'x':
            flags |= SMALL;
        case 'X':
            // flags |= SPECIAL;
            if (qualifier == 'l')
                str = write_num(str, va_arg(args, int64_t), 16, field_width, precision, flags);
            else
                str = write_num(str, va_arg(args, int), 16, field_width, precision, flags);
            break;

        // 打印十进制有符号整数
        case 'i':
        case 'd':

            flags |= SIGN;
            if (qualifier == 'l')
                str = write_num(str, va_arg(args, long long), 10, field_width, precision, flags);
            else
                str = write_num(str, va_arg(args, int), 10, field_width, precision, flags);
            break;

        // 打印十进制无符号整数
        case 'u':
            if (qualifier == 'l')
                str = write_num(str, va_arg(args, unsigned long long), 10, field_width, precision, flags);
            else
                str = write_num(str, va_arg(args, unsigned int), 10, field_width, precision, flags);
            break;

        // 输出有效字符数量到*ip对应的变量
        case 'n':

            if (qualifier == 'l')
                ip = va_arg(args, long long *);
            else
                ip = (int64_t *)va_arg(args, int *);

            *ip = str - buf;
            break;
        case 'f':
            // 默认精度为3
            if (precision < 0)
                precision = 3;

            str = write_float_point_num(str, va_arg(args, double), field_width, precision, flags);

            break;

        // 对于不识别的控制符，直接输出
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

    // 返回缓冲区已有字符串的长度。
    return str - buf;
}

static char *write_num(char *str, uint64_t num, int base, int field_width, int precision, int flags)
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
        num = llabs(num);
        // 进制转换
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
            *str++ = digits[24]; // 注意这里是英文字母O或者o
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

    int js_num_z = 0, js_num_d = 0;   // 临时数字字符串tmp_num_z tmp_num_d的长度
    uint64_t num_z = (uint64_t)(num); // 获取整数部分
    uint64_t num_decimal = (uint64_t)(round(1.0 * (num - num_z) * pow(10, precision))); // 获取小数部分

    if (num == 0 || num_z == 0)
        tmp_num_z[js_num_z++] = '0';
    else
    {
        // 存储整数部分
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