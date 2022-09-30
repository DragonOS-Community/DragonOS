#include <common/glib.h>
#include <common/string.h>

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

/**
 * @brief 将数据从src搬运到dst，并能正确处理地址重叠的问题
 * 
 * @param dst 目标地址指针
 * @param src 源地址指针
 * @param size 大小
 * @return void* 指向目标地址的指针
 */
void *memmove(void *dst, const void *src, uint64_t size)
{
    const char *_src = src;
	char *_dst = dst;

	if (!size)
		return dst;

	// 当源地址大于目标地址时，使用memcpy来完成
	if (dst <= src)
		return memcpy(dst, src, size);

	// 当源地址小于目标地址时，为防止重叠覆盖，因此从后往前拷贝
	_src += size;
	_dst += size;

	// 逐字节拷贝
	while (size--)
		*--_dst = *--_src;

	return dst;
}