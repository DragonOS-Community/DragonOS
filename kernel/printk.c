//
// Created by longjin on 2022/1/22.
//
#include "printk.h"

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

        default:
            break;
        }
    }
}