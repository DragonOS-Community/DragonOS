#include <common/stdlib.h>

/**
 * @brief 将长整型转换为字符串
 *
 * @param input 输入的数据
 * @return const char* 结果字符串
 */
const char *ltoa(long input)
{
    /* large enough for -9223372036854775808 */
    static char buffer[21] = {0};
    char *pos = buffer + sizeof(buffer) - 1;
    int neg = input < 0;
    unsigned long n = neg ? -input : input;

    *pos-- = '\0';
    do
    {
        *pos-- = '0' + n % 10;
        n /= 10;
        if (pos < buffer)
            return pos + 1;
    } while (n);

    if (neg)
        *pos-- = '-';
    return pos + 1;
}