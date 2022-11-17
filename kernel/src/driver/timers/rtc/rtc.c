#include "rtc.h"
#include <common/kprint.h>

/*置位0x70的第7位，禁止不可屏蔽中断*/

#define read_cmos(addr) ({                                                          \
    io_out8(0x70, 0x80 | addr);  \
    io_in8(0x71);                                                                   \
})

enum CMOSTimeSelector
{
    T_SECOND = 0x0,
    T_MINUTE = 0x2,
    T_HOUR = 0x4,
    T_DAY = 0x7,
    T_MONTH = 0x8,
    T_YEAR = 0x9,
};


int rtc_get_cmos_time(struct rtc_time_t *t)
{
    // 为防止中断请求打断该过程，需要先关中断
    cli();

    uint8_t status_register_B = read_cmos(0x0B);                  // 读取状态寄存器B
    bool is_24h = ((status_register_B & 0x02) ? true : false);    // 判断是否启用24小时模式
    bool is_binary = ((status_register_B & 0x04) ? true : false); // 判断是否为二进制码

    do
    {
        t->year = read_cmos(0x09);
        t->month = read_cmos(0x08);
        t->day = read_cmos(0x07);
        t->hour = read_cmos(0x04);
        t->minute = read_cmos(0x02);
        t->second = read_cmos(0x00);
    } while (t->second != read_cmos(0x00)); // 若读取时间过程中时间发生跳变则重新读取
    // 使能NMI中断
    io_out8(0x70, 0x00);

    if (!is_binary) // 把BCD转为二进制
    {
        t->second = (t->second & 0xf) + (t->second >> 4) * 10;
        t->minute = (t->minute & 0xf) + (t->minute >> 4) * 10;
        t->hour = ((t->hour & 0xf) + ((t->hour & 0x70) >> 4) * 10) | (t->hour & 0x80);

        t->month = (t->month & 0xf) + (t->month >> 4) * 10;
        t->year = (t->year & 0xf) + (t->year >> 4) * 10;
    }
    t->year += 2000;

    if ((!is_24h) && t->hour & 0x80) // 将十二小时制转为24小时
        t->hour = ((t->hour & 0x7f) + 12) % 24;
    sti();
    return 0;
}
