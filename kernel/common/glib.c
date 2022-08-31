#include "glib.h"
#include "string.h"

/**
 * @brief 这个函数让蜂鸣器发声，目前仅用于真机调试。未来将移除，请勿依赖此函数。
 * 
 * @param times 发声循环多少遍
 */
void __experimental_beep(uint64_t times)
{
    io_out8(0x43, 182&0xff);
    io_out8(0x42, 2280&0xff);
    io_out8(0x42, (2280>>8)&0xff);
    uint32_t x = io_in8(0x61)&0xff;
    x |= 3;
    io_out8(0x61, x&0xff);

    times *= 10000;
    for(uint64_t i=0;i<times;++i)
        pause();
    x = io_in8(0x61);
    x &= 0xfc;
    io_out8(0x61, x&0xff);

    // 延迟一段时间
    for(uint64_t i=0;i<times;++i)
        pause();
}