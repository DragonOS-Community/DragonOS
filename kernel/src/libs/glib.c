#include <common/glib.h>
#include <common/string.h>


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