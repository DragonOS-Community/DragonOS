#include "multiboot2.h"

#include <common/glib.h>
#include <common/kprint.h>
// uintptr_t multiboot2_boot_info_addr;
// unsigned int multiboot2_magic;
unsigned int multiboot2_boot_info_size;

#define MBI_RAW_MAX_SIZE 409600
// 由于启动时传递的mb2 info所在的地址,在内存管理初始化之后会被覆盖，所以需要将其拷贝到一个固定的位置
static uint8_t mbi_raw[MBI_RAW_MAX_SIZE] = {0};
bool multiboot2_init(uint64_t mb2_info_paddr, uint32_t mb2_magic)
{
  uint64_t vaddr = (uint64_t)phys_2_virt(mb2_info_paddr);
  if (mb2_magic != MULTIBOOT2_BOOTLOADER_MAGIC)
    return false;
  // vaddr+0 处保存了大小
  multiboot2_boot_info_size = *(uint32_t *)vaddr;
  if (multiboot2_boot_info_size > MBI_RAW_MAX_SIZE)
    return false;

  memcpy((void *)mbi_raw, (void *)vaddr, multiboot2_boot_info_size);
  
  return true;
}

void multiboot2_iter(bool (*_fun)(const struct iter_data_t *, void *, unsigned int *),
                     void *data, unsigned int *count)
{
  // kdebug("multiboot2_boot_info_addr=%#018lx", multiboot2_boot_info_addr);

  // uintptr_t addr = multiboot2_boot_info_addr;

  // for(int i=0;i<8192;i++)
  // {
  //   mbi_raw[i] = ((uint8_t *)multiboot2_boot_info_addr)[i];
  // }
  uint8_t * addr = mbi_raw;
  // 接下来的第8字节开始，为 tag 信息
  struct iter_data_t *tag = (struct iter_data_t *)((void *)addr + 8);
  for (; tag->type != MULTIBOOT_TAG_TYPE_END;
       tag = (struct iter_data_t *)((uint8_t *)tag + ALIGN(tag->size, 8)))
  {

    if (_fun(tag, data, count) == true)
    {
      return;
    }
  }
  return;
}

// 读取 grub2 传递的物理内存信息，保存到 e820map_t 结构体中
// 一般而言是这样的
// 地址(长度) 类型
// 0x00(0x9F000) 0x1
// 0x9F000(0x1000) 0x2
// 0xE8000(0x18000) 0x2
// 0x100000(0x7EF0000) 0x1
// 0x7FF0000(0x10000) 0x3
// 0xFFFC0000(0x40000) 0x2
/**
 * @brief 获取multiboot2协议提供的内存区域信息
 *
 * @param _iter_data 要被迭代的信息的结构体
 * @param _data 返回信息的结构体指针
 * @param count 返回数组的长度
 * @return true
 * @return false
 */
bool multiboot2_get_memory(const struct iter_data_t *_iter_data, void *data, unsigned int *count)
{
  if (_iter_data->type != MULTIBOOT_TAG_TYPE_MMAP)
    return false;

  struct multiboot_mmap_entry_t *resource = (struct multiboot_mmap_entry_t *)data;
  struct multiboot_mmap_entry_t *mmap = ((struct multiboot_tag_mmap_t *)_iter_data)->entries;
  *count = 0;
  for (; (uint8_t *)mmap < (uint8_t *)_iter_data + _iter_data->size;
       mmap = (struct multiboot_mmap_entry_t *)((uint8_t *)mmap + ((struct multiboot_tag_mmap_t *)_iter_data)->entry_size))
  {
    *resource = *mmap;
    // 将指针进行增加
    resource = (struct multiboot_mmap_entry_t *)((uint8_t *)resource + ((struct multiboot_tag_mmap_t *)_iter_data)->entry_size);
    ++(*count);
  }
  return true;
}

/**
 * @brief 获取VBE信息
 *
 * @param _iter_data 要被迭代的信息的结构体
 * @param _data 返回信息的结构体指针
 */
bool multiboot2_get_VBE_info(const struct iter_data_t *_iter_data, void *data, unsigned int *reserved)
{

  if (_iter_data->type != MULTIBOOT_TAG_TYPE_VBE)
    return false;
  *(struct multiboot_tag_vbe_t *)data = *(struct multiboot_tag_vbe_t *)_iter_data;
  return true;
}

/// @brief 获取加载基地址
/// @param _iter_data 
/// @param data 
/// @param reserved 
/// @return 
bool multiboot2_get_load_base(const struct iter_data_t *_iter_data, void *data, unsigned int *reserved)
{

  if (_iter_data->type != MULTIBOOT_TAG_TYPE_LOAD_BASE_ADDR)
    return false;
  *(struct multiboot_tag_load_base_addr_t *)data = *(struct multiboot_tag_load_base_addr_t *)_iter_data;
  return true;
}

/**
 * @brief 获取帧缓冲区信息
 *
 * @param _iter_data 要被迭代的信息的结构体
 * @param _data 返回信息的结构体指针
 */
bool multiboot2_get_Framebuffer_info(const struct iter_data_t *_iter_data, void *data, unsigned int *reserved)
{
  if (_iter_data->type != MULTIBOOT_TAG_TYPE_FRAMEBUFFER)
    return false;
  *(struct multiboot_tag_framebuffer_info_t *)data = *(struct multiboot_tag_framebuffer_info_t *)_iter_data;
  return true;
}

/**
 * @brief 获取acpi旧版RSDP
 *
 * @param _iter_data 要被迭代的信息的结构体
 * @param _data old RSDP的结构体指针
 * @param reserved
 * @return uint8_t*  struct multiboot_tag_old_acpi_t
 */
bool multiboot2_get_acpi_old_RSDP(const struct iter_data_t *_iter_data, void *data, unsigned int *reserved)
{
  if (_iter_data->type != MULTIBOOT_TAG_TYPE_ACPI_OLD)
    return false;

  *(struct multiboot_tag_old_acpi_t *)data = *(struct multiboot_tag_old_acpi_t *)_iter_data;

  return true;
}

/**
 * @brief 获取acpi新版RSDP
 *
 * @param _iter_data 要被迭代的信息的结构体
 * @param _data old RSDP的结构体指针
 * @param reserved
 * @return uint8_t*  struct multiboot_tag_old_acpi_t
 */
bool multiboot2_get_acpi_new_RSDP(const struct iter_data_t *_iter_data, void *data, unsigned int *reserved)
{
  if (_iter_data->type != MULTIBOOT_TAG_TYPE_ACPI_NEW)
    return false;
  *(struct multiboot_tag_new_acpi_t *)data = *(struct multiboot_tag_new_acpi_t *)_iter_data;
  return true;
}