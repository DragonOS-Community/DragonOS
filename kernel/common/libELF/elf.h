#pragma once
#include <common/glib.h>

// Reference: https://docs.oracle.com/cd/E23824_01/html/819-0690/chapter6-43405.html#scrolltoc

// ====== ELF32 Header中的数据类型定义 ====
typedef uint32_t Elf32_Addr;
typedef uint16_t Elf32_Half;
typedef uint32_t Elf32_Off;
typedef uint32_t Elf32_SWord;
typedef uint32_t Elf32_Word;

// ====== ELF64 Header中的数据类型定义 ====
typedef uint64_t Elf64_Addr;
typedef uint16_t Elf64_Half;
typedef uint64_t Elf64_Off;
typedef uint32_t Elf64_Sword;
typedef uint32_t Elf64_Word;
typedef uint64_t Elf64_Xword;
typedef uint64_t Elf64_Sxword;

// ELF Header中的最大段entry数量
#define EI_NIDENT 16

// ELF e_type的类型定义
#define ET_NONE 0        // No file type
#define ET_REL 1         // Relocatable file
#define ET_EXEC 2        // Executable file
#define ET_DYN 3         // Shared object file
#define ET_CORE 4        // Core file
#define ET_LOPROC 0xff00 // Processor-specific
#define ET_HIPROC 0xffff // Processor-specific

// e_machine的类型定义
#define EM_NONE 0         // No machine
#define EM_SPARC 2        // SPARC
#define EM_386 3          // Intel 80386
#define EM_SPARC32PLUS 18 // Sun SPARC 32+
#define EM_SPARCV9 43     // SPARC V9
#define EM_AMD64 62       // AMD 64

// e_version的类型定义
#define EV_NONE 0 // Invalid Version
// EV_CURRENT: Value>=1 means current version

// e_flags 定义
// e_flags for SPARC
#define EF_SPARC_EXT_MASK 0xffff00 // Vendor Extension mask
#define EF_SPARC_32PLUS 0x000100   // Generic V8+ features
#define EF_SPARC_SUN_US1 0x000200  // Sun UltraSPARC 1 Extensions
#define EF_SPARC_HAL_R1 0x000400   // HAL R1 Extensions
#define EF_SPARC_SUN_US3 0x000800  // Sun UltraSPARC 3 Extensions
#define EF_SPARCV9_MM 0x3          // Mask for Memory Model
#define EF_SPARCV9_TSO 0x0         // Total Store Ordering
#define EF_SPARCV9_PSO 0x1         // Partial Store Ordering
#define EF_SPARCV9_RMO 0x2         // Relaxed Memory Ordering

#define PN_XNUM 0xffff
#define SHN_LORESERVE 0xff00
#define SHN_XINDEX 0xffff

typedef struct
{
    unsigned char e_ident[EI_NIDENT];
    Elf32_Half e_type;
    Elf32_Half e_machine;
    Elf32_Word e_version;
    Elf32_Addr e_entry;
    Elf32_Off e_phoff;
    Elf32_Off e_shoff;
    Elf32_Word e_flags;
    Elf32_Half e_ehsize;
    Elf32_Half e_phentsize;
    Elf32_Half e_phnum;
    Elf32_Half e_shentsize;
    Elf32_Half e_shnum;
    Elf32_Half e_shstrndx;
} Elf32_Ehdr;

typedef struct
{
    unsigned char e_ident[EI_NIDENT];
    Elf64_Half e_type;    // 文件类型标志符
    Elf64_Half e_machine; // 该文件依赖的处理器架构类型
    Elf64_Word e_version; // 对象文件的版本
    Elf64_Addr e_entry;   // 进程的虚拟地址入点，使用字节偏移量表示。如果没有entry point，则该值为0
    Elf64_Off e_phoff;    // The program header table's file offset in bytes. 若没有，则为0
    Elf64_Off e_shoff;    // The section header table's file offset in bytes. 若没有，则为0
    Elf64_Word e_flags;   // 与处理器相关联的flags。格式为： EF_machine_flag  如果是x86架构，那么该值为0
    Elf64_Half e_ehsize;  // ELF Header的大小（单位：字节）
    Elf64_Half e_phentsize; // 程序的program header table中的一个entry的大小（所有的entry大小相同）
    Elf64_Half e_phnum; // program header table的entry数量
                        // e_phentsize*e_phnum=program header table的大小
                        // 如果没有program header table，该值为0
                        // 如果entry num>=PN_XNUM(0xffff), 那么该值为0xffff，且真实的pht的entry数量存储在section header的sh_info中（index=0）
                        //  其他情况下，第一个section header entry的sh_info的值为0
    
    Elf64_Half e_shentsize; // 每个section header的大小（字节
                            // 每个section header是section header table的一个entry
    
    Elf64_Half e_shnum; // section header table的entry数量
                        // e_shentsize*e_shnum=section header table的大小
                        // 如果没有section header table，那么该值为0
                        // 如果section的数量>=SHN_LORESERVE(0xff00)，那么该值为0，且真实的section数量存储在
                        // section header at index 0的sh_size变量中，否则第一个sh_size为0
    
    Elf64_Half e_shstrndx; // 与section name string表相关联的section header table的entry的索引下标
                            // 如果没有name string table,那么该值等于SHN_UNDEF
                            // 如果对应的index>=SHN_LORESERVE(0xff00)， 那么该变量值为SHN_XINDEX(0xffff)
                            // 且真正的section name string table的index被存放在section header的index=0处的sh_link变量中
                            // 否则初始section header entry的sh_link变量为0
} Elf64_Ehdr;
