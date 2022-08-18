#include "mmio.h"
#include "mmio-buddy.h"

void mmio_init()
{
    mmio_buddy_init();
}

/**
 * @brief 创建一块mmio区域，并将vma绑定到initial_mm
 *
 * @param size mmio区域的大小（字节）
 * @param length mmio区域长度
 * @param vm_flags 要把vma设置成的标志
 * @param res_vaddr 返回值-分配得到的虚拟地址
 * @param res_length 返回值-分配的虚拟地址空间长度
 * @return int 错误码
 */
int mmio_create(uint32_t size, uint64_t length, vm_flags_t vm_flags, uint64_t * res_vaddr, uint64_t *res_length)
{
    
}

/**
 * @brief 取消mmio的映射并将地址空间归还到buddy中
 * 
 * @param vaddr 起始的虚拟地址
 * @param length 要归还的地址空间的长度
 * @return int 错误码
 */
int mmio_release(uint64_t vaddr, uint64_t length)
{

}