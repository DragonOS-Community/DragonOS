//
// Created by longjin on 2022/1/22.
//
#include "printk.h"
#include <math.h>

int skip_and_atoi(const char **s)
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
static int vsprintf(char *buf, const char *fmt, va_list args)
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
        ++fmt;

        bool flag_tmp = true;
        bool flag_break = false;

        while (flag_tmp)
        {
            switch (*fmt)
            {
            case '\0':
                //结束解析
                flag_break = true;
                flag_tmp = false;
                break;
            case '%':
                //输出 %
                *str = '%';
                ++str;
                ++fmt;
                flag_break = true;
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
        if (*fmt == '*')
        {
            field_width = va_arg(args, int);
            ++fmt;
        }
        else if (is_digit(*fmt))
            field_width = skip_and_atoi(&fmt);

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

        //转化成字符串

        switch (*fmt)
        {
        // 显示一个字符
        case 'c':
            //靠右对齐
            if (!(flags & LEFT))
            {
                while (--field_width)
                {
                    *str = ' ';
                    ++str;
                }
            }
            else //靠左对齐
            {
                *str = (char)va_arg(args, int);
                ++str;
                --field_width;
            }
            while (--field_width)
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
            if (qualifier == 'l')
                write_num(str, va_arg(args, long long), 8, field_width, precision, flags);
            else
                write_num(str, va_arg(args, int), 8, field_width, precision, flags);
            break;

        //打印指针指向的地址
        case 'p':
            if (field_width == 0)
            {
                field_width = 2 * sizeof(void *);
                flags |= PAD_ZERO;
            }

            write_num(str, (unsigned long)va_arg(args, void *), 16, field_width, precision, flags);

            break;

        //打印十六进制
        case 'x':
            flags |= SMALL;
        case 'X':
            if (qualifier == 'l')
                write_num(str, va_arg(args, long long), 16, field_width, precision, flags);
            else
                write_num(str, va_arg(args, int), 16, field_width, precision, flags);
            break;

        //打印十进制有符号整数
        case 'i':
        case 'd':
        case 'ld':
            flags |= SIGN;
            if (qualifier == 'l')
                write_num(str, va_arg(args, long long), 10, field_width, precision, flags);
            else
                write_num(str, va_arg(args, int), 10, field_width, precision, flags);
            break;

        //打印十进制无符号整数
        case 'u':
            if (qualifier == 'l')
                write_num(str, va_arg(args, unsigned long long), 10, field_width, precision, flags);
            else
                write_num(str, va_arg(args, unsigned int), 10, field_width, precision, flags);
            break;

        //输出有效字符数量到*ip对应的变量
        case 'n':
            long long *ip;
            if (qualifier == 'l')
                ip = va_arg(args, long long *);
            else
                ip = va_arg(args, int *);

            *ip = str - buf;
            break;

        //对于不识别的控制符，直接输出
        default:
            *str++ = '%';
            if(*fmt)
                *str++ = *fmt;
            else --fmt;
            break;
        }
    }
    *str = '\0';
    //返回缓冲区已有字符串的长度。
    return str-buf;
}

static void write_num(char *str, long long num, int base, int field_width, int precision, int flags)
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
        num = abs(num);
        //进制转换
        while (num)
        {
            tmp_num[js_num++] = num % base; // 注意这里，输出的数字，是小端对齐的。低位存低位
            num /= base;
        }
    }

    if (js_num > precision)
        precision = js_num;

    field_width -= precision;

    // 靠右对齐
    if (!(flags & LEFT))
        while (field_width--)
            *str++ = pad;

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

    while (js_num < precision)
    {
        --precision;
        *str++ = '0';
    }

    while (js_num--)
        *str++ = tmp_num[js_num];

    while (field_width--)
        *str++ = ' ';

    return str;
}