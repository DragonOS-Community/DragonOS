/**
 * @file multiboot2.h
 * @brief multiboot2 解析
 */

#pragma once

#include <DragonOS/stdint.h>
#include "stdbool.h"
#include <common/boot_info.h>
#include <driver/acpi/acpi.h>

/// @see Multiboot2 Specification version 2.0.pdf
// 启动后，在 32 位内核进入点，机器状态如下：
//   1. CS 指向基地址为 0x00000000，限长为4G – 1的代码段描述符。
//   2. DS，SS，ES，FS 和 GS 指向基地址为0x00000000，限长为4G –
//   1的数据段描述符。
//   3. A20 地址线已经打开。
//   4. 页机制被禁止。
//   5. 中断被禁止。
//   6. EAX = 0x2BADB002
//   7. 系统信息和启动信息块的线性地址保存在 EBX中（相当于一个指针）。
//      以下即为这个信息块的结构

/**
 * @brief MULTIBOOT2 接口抽象
 */

/*  How many bytes from the start of the file we search for the header. */
static const unsigned int MULTIBOOT_SEARCH = 32768;
static const unsigned int MULTIBOOT_HEADER_ALIGN = 8;

/*  The magic field should contain this. */
static const unsigned int MULTIBOOT2_HEADER_MAGIC = 0xe85250d6;

/*  This should be in %eax. */
static const unsigned int MULTIBOOT2_BOOTLOADER_MAGIC = 0x36d76289;

/*  Alignment of multiboot modules. */
static const unsigned int MULTIBOOT_MOD_ALIGN = 0x00001000;

/*  Alignment of the multiboot info structure. */
static const unsigned int MULTIBOOT_INFO_ALIGN = 0x00000008;

/*  Flags set in the 'flags' member of the multiboot header. */

static const unsigned int MULTIBOOT_TAG_ALIGN = 8;
static const unsigned int MULTIBOOT_TAG_TYPE_END = 0;
static const unsigned int MULTIBOOT_TAG_TYPE_CMDLINE = 1;
static const unsigned int MULTIBOOT_TAG_TYPE_BOOT_LOADER_NAME = 2;
static const unsigned int MULTIBOOT_TAG_TYPE_MODULE = 3;
static const unsigned int MULTIBOOT_TAG_TYPE_BASIC_MEMINFO = 4;
static const unsigned int MULTIBOOT_TAG_TYPE_BOOTDEV = 5;
static const unsigned int MULTIBOOT_TAG_TYPE_MMAP = 6;
static const unsigned int MULTIBOOT_TAG_TYPE_VBE = 7;
static const unsigned int MULTIBOOT_TAG_TYPE_FRAMEBUFFER = 8;
static const unsigned int MULTIBOOT_TAG_TYPE_ELF_SECTIONS = 9;
static const unsigned int MULTIBOOT_TAG_TYPE_APM = 10;
static const unsigned int MULTIBOOT_TAG_TYPE_EFI32 = 11;
static const unsigned int MULTIBOOT_TAG_TYPE_EFI64 = 12;
static const unsigned int MULTIBOOT_TAG_TYPE_SMBIOS = 13;
static const unsigned int MULTIBOOT_TAG_TYPE_ACPI_OLD = 14;
static const unsigned int MULTIBOOT_TAG_TYPE_ACPI_NEW = 15;
static const unsigned int MULTIBOOT_TAG_TYPE_NETWORK = 16;
static const unsigned int MULTIBOOT_TAG_TYPE_EFI_MMAP = 17;
static const unsigned int MULTIBOOT_TAG_TYPE_EFI_BS = 18;
static const unsigned int MULTIBOOT_TAG_TYPE_EFI32_IH = 19;
static const unsigned int MULTIBOOT_TAG_TYPE_EFI64_IH = 20;
static const unsigned int MULTIBOOT_TAG_TYPE_LOAD_BASE_ADDR = 21;

static const unsigned int MULTIBOOT_HEADER_TAG_END = 0;
static const unsigned int MULTIBOOT_HEADER_TAG_INFORMATION_REQUEST =
    1;
static const unsigned int MULTIBOOT_HEADER_TAG_ADDRESS = 2;
static const unsigned int MULTIBOOT_HEADER_TAG_ENTRY_ADDRESS = 3;
static const unsigned int MULTIBOOT_HEADER_TAG_CONSOLE_FLAGS = 4;
static const unsigned int MULTIBOOT_HEADER_TAG_FRAMEBUFFER = 5;
static const unsigned int MULTIBOOT_HEADER_TAG_MODULE_ALIGN = 6;
static const unsigned int MULTIBOOT_HEADER_TAG_EFI_BS = 7;
static const unsigned int MULTIBOOT_HEADER_TAG_ENTRY_ADDRESS_EFI32 =
    8;
static const unsigned int MULTIBOOT_HEADER_TAG_ENTRY_ADDRESS_EFI64 =
    9;
static const unsigned int MULTIBOOT_HEADER_TAG_RELOCATABLE = 10;

static const unsigned int MULTIBOOT_ARCHITECTURE_I386 = 0;
static const unsigned int MULTIBOOT_ARCHITECTURE_MIPS32 = 4;
static const unsigned int MULTIBOOT_HEADER_TAG_OPTIONAL = 1;

static const unsigned int MULTIBOOT_LOAD_PREFERENCE_NONE = 0;
static const unsigned int MULTIBOOT_LOAD_PREFERENCE_LOW = 1;
static const unsigned int MULTIBOOT_LOAD_PREFERENCE_HIGH = 2;

static const unsigned int MULTIBOOT_CONSOLE_FLAGS_CONSOLE_REQUIRED =
    1;
static const unsigned int MULTIBOOT_CONSOLE_FLAGS_EGA_TEXT_SUPPORTED =
    2;

static const unsigned int MULTIBOOT_MEMORY_AVAILABLE = 1;
static const unsigned int MULTIBOOT_MEMORY_RESERVED = 2;
static const unsigned int MULTIBOOT_MEMORY_ACPI_RECLAIMABLE = 3;
static const unsigned int MULTIBOOT_MEMORY_NVS = 4;
static const unsigned int MULTIBOOT_MEMORY_BADRAM = 5;

struct multiboot_header_t
{
    // Must be MULTIBOOT_MAGIC - see above.
    unsigned int magic;
    // ISA
    unsigned int architecture;
    // Total header length.
    unsigned int header_length;
    // The above fields plus this one must equal 0 mod 2^32.
    unsigned int checksum;
};

struct multiboot_header_tag_t
{
    uint16_t type;
    uint16_t flags;
    unsigned int size;
};
struct multiboot_header_tag_information_request_t
{
    uint16_t type;
    uint16_t flags;
    unsigned int size;

    unsigned int requests[0];
};
struct multiboot_header_tag_address_t
{
    uint16_t type;
    uint16_t flags;
    unsigned int size;

    unsigned int header_addr;
    unsigned int load_addr;
    unsigned int load_end_addr;
    unsigned int bss_end_addr;
};

struct multiboot_header_tag_entry_address_t
{
    uint16_t type;
    uint16_t flags;
    unsigned int size;

    unsigned int entry_addr;
};

struct multiboot_header_tag_console_flags_t
{
    uint16_t type;
    uint16_t flags;
    unsigned int size;

    unsigned int console_flags;
};

struct multiboot_header_tag_framebuffer_t
{
    uint16_t type;
    uint16_t flags;
    unsigned int size;

    unsigned int width;
    unsigned int height;
    unsigned int depth;
};

struct multiboot_header_tag_module_align_t
{
    uint16_t type;
    uint16_t flags;
    unsigned int size;
};

struct multiboot_header_tag_relocatable_t
{
    uint16_t type;
    uint16_t flags;
    unsigned int size;

    unsigned int min_addr;
    unsigned int max_addr;
    unsigned int align;
    unsigned int preference;
};

struct multiboot_color_t
{
    uint8_t red;
    uint8_t green;
    uint8_t blue;
};

// multiboot2协议的内存区域信息
struct multiboot_mmap_entry_t
{
    uint64_t addr;
    uint64_t len;
    unsigned int type;
    unsigned int reserved;
};

struct multiboot_tag_t
{
    unsigned int type;
    unsigned int size;
};

struct multiboot_tag_string_t
{
    struct multiboot_tag_t tag_t;
    char string[0];
};

struct multiboot_tag_module_t
{
    struct multiboot_tag_t tag_t;
    unsigned int mod_start;
    unsigned int mod_end;
    char cmdline[0];
};

struct multiboot_tag_basic_meminfo_t
{
    struct multiboot_tag_t tag_t;
    unsigned int mem_lower;
    unsigned int mem_upper;
};

struct multiboot_tag_bootdev_t
{
    struct multiboot_tag_t tag_t;
    unsigned int biosdev;
    unsigned int slice;
    unsigned int part;
};

struct multiboot_tag_mmap_t
{
    struct multiboot_tag_t tag_t;
    unsigned int entry_size;
    unsigned int entry_version;
    struct multiboot_mmap_entry_t entries[0];
};

struct multiboot_vbe_info_block_t
{
    uint8_t external_specification[512];
};

struct multiboot_vbe_mode_info_block_t
{
    uint8_t external_specification[256];
};

// bootloader传递的VBE信息的结构体
struct multiboot_tag_vbe_t
{
    struct multiboot_tag_t tag_t;
    uint16_t vbe_mode;
    uint16_t vbe_interface_seg;
    uint16_t vbe_interface_off;
    uint16_t vbe_interface_len;

    // The fields ‘vbe_control_info’ and ‘vbe_mode_info’ contain VBE control information returned by the VBE Function 00h and VBE mode information
    // returned by the VBE Function 01h, respectively.
    struct multiboot_vbe_info_block_t vbe_control_info;
    struct multiboot_vbe_mode_info_block_t vbe_mode_info;
};

struct multiboot_tag_framebuffer_info_t
{
    struct multiboot_tag_t tag_t;
    uint64_t framebuffer_addr;
    uint32_t framebuffer_pitch; // 帧缓存上界
    // width and height expressed in pixels except type=2
    // when type=2, they are expressed in characters
    uint32_t framebuffer_width;
    uint32_t framebuffer_height;
    // number of bits per pixel.
    uint8_t framebuffer_bpp;
    // 帧缓存的类型
    uint8_t framebuffer_type;
    uint8_t reserved;
};

// indexed color
struct multiboot_tag_framebuffer_info_type0_t
{
    struct multiboot_tag_framebuffer_info_t header;
    uint32_t framebuffer_palette_num_colors;
    struct multiboot_color_t color_desc;
};

// direct RGB color
struct multiboot_tag_framebuffer_info_type1_t
{
    struct multiboot_tag_framebuffer_info_t header;

    uint8_t framebuffer_red_field_position;
    uint8_t framebuffer_red_mask_size;
    uint8_t framebuffer_green_field_position;
    uint8_t framebuffer_green_mask_size;
    uint8_t framebuffer_blue_field_position;
    uint8_t framebuffer_blue_mask_size;
};

struct multiboot_tag_elf_sections_t
{
    struct multiboot_tag_t tag_t;
    unsigned int num;
    unsigned int entsize;
    // 段字符串表索引
    unsigned int shndx;
    char sections[0];
};

struct multiboot_tag_apm_t
{
    struct multiboot_tag_t tag_t;
    uint16_t version;
    uint16_t cseg;
    unsigned int offset;
    uint16_t cseg_16;
    uint16_t dseg;
    uint16_t flags;
    uint16_t cseg_len;
    uint16_t cseg_16_len;
    uint16_t dseg_len;
};

struct multiboot_tag_efi32_t
{
    struct multiboot_tag_t tag_t;
    unsigned int pointer;
};

struct multiboot_tag_efi64_t
{
    struct multiboot_tag_t tag_t;
    uint64_t pointer;
};

struct multiboot_tag_smbios_t
{
    struct multiboot_tag_t tag_t;
    uint8_t major;
    uint8_t minor;
    uint8_t reserved[6];
    uint8_t tables[0];
};

struct multiboot_tag_old_acpi_t
{
    struct multiboot_tag_t tag_t;
    //uint8_t rsdp[0];
    struct acpi_RSDP_t rsdp;
};

struct multiboot_tag_new_acpi_t
{
    struct multiboot_tag_t tag_t;
    //uint8_t rsdp[0];
    struct acpi_RSDP_2_t rsdp;
};

struct multiboot_tag_network_t
{
    struct multiboot_tag_t tag_t;
    uint8_t dhcpack[0];
};

struct multiboot_tag_efi_mmap_t
{
    struct multiboot_tag_t tag_t;
    unsigned int descr_size;
    unsigned int descr_vers;
    uint8_t efi_mmap[0];
};

struct multiboot_tag_efi32_ih_t
{
    struct multiboot_tag_t tag_t;
    unsigned int pointer;
};

struct multiboot_tag_efi64_ih_t
{
    struct multiboot_tag_t tag_t;
    uint64_t pointer;
};

struct multiboot_tag_load_base_addr_t
{
    struct multiboot_tag_t tag_t;
    unsigned int load_base_addr;
};

// 迭代变量
// 与 multiboot_tag_t 相同
struct iter_data_t
{
    unsigned int type;
    unsigned int size;
};

/**
 * @brief 初始化
 * @return true            成功
 * @return false           失败
 */
bool multiboot2_init(uint64_t mb2_info_paddr, uint32_t mb2_magic);

/**
 * @brief 迭代器
 * @param  _fun            迭代操作
 * @param  _data           数据
 */
void multiboot2_iter(bool (*_fun)(const struct iter_data_t *, void *, unsigned int *),
                     void *_data, unsigned int *count);

/**
 * @brief 获取multiboot2协议提供的内存区域信息
 *
 * @param _iter_data 要被迭代的信息的结构体
 * @param _data 返回信息的结构体指针
 * @param count 返回数组的长度
 * @return true
 * @return false
 */
bool multiboot2_get_memory(const struct iter_data_t *_iter_data, void *_data, unsigned int *count);

bool multiboot2_get_load_base(const struct iter_data_t *_iter_data, void *data, unsigned int *reserved);

/**
 * @brief 获取VBE信息
 *
 * @param _iter_data 要被迭代的信息的结构体
 * @param _data 返回信息的结构体指针
 */
bool multiboot2_get_VBE_info(const struct iter_data_t *_iter_data, void *_data, unsigned int *reserved);

/**
 * @brief 获取帧缓冲区信息
 *
 * @param _iter_data 要被迭代的信息的结构体
 * @param _data 返回信息的结构体指针
 */
bool multiboot2_get_Framebuffer_info(const struct iter_data_t *_iter_data, void *_data, unsigned int *reserved);

/**
 * @brief 获取acpi旧版RSDP
 *
 * @param _iter_data 要被迭代的信息的结构体
 * @param _data old RSDP的结构体指针
 * @param reserved
 * @return uint8_t*
 */
bool multiboot2_get_acpi_old_RSDP(const struct iter_data_t *_iter_data, void *data, unsigned int *reserved);

/**
 * @brief 获取acpi新版RSDP
 *
 * @param _iter_data 要被迭代的信息的结构体
 * @param _data old RSDP的结构体指针
 * @param reserved
 * @return uint8_t*  struct multiboot_tag_old_acpi_t
 */
bool multiboot2_get_acpi_new_RSDP(const struct iter_data_t *_iter_data, void *data, unsigned int *reserved);