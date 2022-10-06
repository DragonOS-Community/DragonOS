#pragma once
#include <common/glib.h>

// --> begin ==============EHDR=====================
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

// ====== ELF Identification Index ======
// Purpose: File identification
#define EI_MAG0 0
#define EI_MAG1 1
#define EI_MAG2 2
#define EI_MAG3 3
// Purpose: File class
#define EI_CLASS 4
// Purpose: Data encoding
#define EI_DATA 5
// Purpose: File version
#define EI_VERSION 6 // e_ident[EI_VERSION]指定了ELF header的版本号 当前这个值必须是EV_CURRENT

// Purpose: Operating system/ABI identification
#define EI_OSABI 7 // e_ident[EI_OSABI]指定了操作系统以及对象所对应的ABI

// Purpose: ABI version
#define EI_ABIVERSION 8 // e_ident[EI_ABIVERSION] 指定了对象所对应的ABI版本.

// Purpose: Start of padding bytes
#define EI_PAD 9 // 这个值标志了e_ident中未使用字节的的起始下标
// Purpose: Size of e_ident[]
#define EI_NIDENT 16

// EI_MAG0 - EI_MAG3 这是一个4byte的 magic number
#define ELFMAG0 0x7f
#define ELFMAG1 'E'
#define ELFMAG2 'L'
#define ELFMAG3 'F'

// EI_CLASS  e_ident[EI_CLASS]指明了文件的类型或capacity
#define ELFCLASSNONE 0 // Invalid class
#define ELFCLASS32 1   // 32–bit objects
#define ELFCLASS64 2   // 64–bit objects

// EI_DATA e_ident[EI_DATA]指明了与处理器相关的数据的编码方式
#define ELFDATANONE 0
#define ELFDATA2LSB 1 //  小端对齐
#define ELFDATA2MSB 2 // 大端对齐

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
    unsigned char e_ident[EI_NIDENT]; // 标志字节，这些字节与机器架构类型无关。目的是为了告诉我们如何解析这个文件的内容
    Elf64_Half e_type;                // 文件类型标志符
    Elf64_Half e_machine;             // 该文件依赖的处理器架构类型
    Elf64_Word e_version;             // 对象文件的版本
    Elf64_Addr e_entry;               // 进程的虚拟地址入点，使用字节偏移量表示。如果没有entry point，则该值为0
    Elf64_Off e_phoff;                // The program header table's file offset in bytes. 若没有，则为0
    Elf64_Off e_shoff;                // The section header table's file offset in bytes. 若没有，则为0
    Elf64_Word e_flags;               // 与处理器相关联的flags。格式为： EF_machine_flag  如果是x86架构，那么该值为0
    Elf64_Half e_ehsize;              // ELF Header的大小（单位：字节）
    Elf64_Half e_phentsize;           // 程序的program header table中的一个entry的大小（所有的entry大小相同）
    Elf64_Half e_phnum;               // program header table的entry数量
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

// --> end ==============EHDR=====================

// --> begin ==============SHDR=====================

// reference: https://docs.oracle.com/cd/E23824_01/html/819-0690/chapter6-94076.html#scrolltoc

// ===== ELF Special Section Indexes =====
#define SHN_UNDEF 0          // An undefined, missing, irrelevant, or otherwise meaningless section reference.
#define SHN_LORESERVE 0xff00 // The lower boundary of the range of reserved indexes.
                             // The system reserves indexes between SHN_LORESERVE and SHN_HIRESERVE, inclusive.
#define SHN_LOPROC 0xff00    // SHN_LOPROC - SHN_HIPROC 这个范围以内的数据为处理器特定的语义所保留
#define SHN_BEFORE 0xff00    // SHN_BEFORE, SHN_AFTER 与SHF_LINK_ORDER及SHF_ORDERED section flags一起，提供初始和终止section的
#define SHN_AFTER 0xff01
#define SHN_AMD64_LCOMMON 0xff02 // x64 specific common block label. This label is similar to SHN_COMMON, but provides for identifying a large common block.
#define SHN_HIPROC 0xff1f
#define SHN_LOOS 0xff20        // SHN_LOOS - SHN_HIOS 这个范围你的数为操作系统特定的语义所保留
#define SHN_LOSUNW 0xff3f      // SHN_LOSUNW - SHN_HISUNW Values in this inclusive range are reserved for Sun-specific semantics.
#define SHN_SUNW_IGNORE 0xff3f // This section index provides a temporary symbol definition within relocatable objects. Reserved for internal use by dtrace(1M).
#define SHN_HISUNW 0xff3f
#define SHN_HIOS 0xff3f
#define SHN_ABS 0xfff1    // 对应的引用的绝对值。 举个例子，symbols defined relative to section number SHN_ABS have absolute values and are not affected by relocation.
#define SHN_COMMON 0xfff2 // Symbols defined relative to this section are common symbols
#define SHN_XINDEX 0xffff
#define SHN_HIRESERVE 0xffff // The upper boundary of the range of reserved indexes.
/*
Note -
    Although index 0 is reserved as the undefined value,
    the section header table contains an entry for index 0.
    That is, if the e_shnum member of the ELF header indicates
    a file has 6 entries in the section header table, the sections
    have the indexes 0 through 5. The contents of the initial entry
    are specified later in this section.
*/

typedef struct
{
    Elf32_Word sh_name;
    Elf32_Word sh_type;
    Elf32_Word sh_flags;
    Elf32_Addr sh_addr;
    Elf32_Off sh_offset;
    Elf32_Word sh_size;
    Elf32_Word sh_link;
    Elf32_Word sh_info;
    Elf32_Word sh_addralign;
    Elf32_Word sh_entsize;
} Elf32_Shdr;

typedef struct
{
    Elf64_Word sh_name; // 段名
    Elf64_Word sh_type; // 段的类型（按照内容和语义来分类）
    Elf64_Xword sh_flags;
    Elf64_Addr sh_addr;       // 该section在进程的内存空间中的基地址。如果该段不需要出现在内存中，该值为0
    Elf64_Off sh_offset;      // The byte offset from the beginning of the file to the first byte in the section
                              // 对于一个 SHT_NOBITS section，这个变量指的是概念上的偏移量。因为这种段并不是真正存在于文件中
    Elf64_Xword sh_size;      // The section's size in bytes(如果是SHT_NOBITS类型的section，section不会在文件中真正占用sh_size的空间)
    Elf64_Word sh_link;       // A section header table index link, whose interpretation depends on the section type.
    Elf64_Word sh_info;       // 依赖于section type来解析的额外的信息。如果sh_flags有SHF_INFO_LINK属性，那么这个变量代表一个section header table index.
    Elf64_Xword sh_addralign; // 地址按照多少bytes对齐。只允许使用2的n次幂的值。如果值为0或1，则意味着地址没有对齐要求。
    Elf64_Xword sh_entsize;   // 如果某个段拥有指定size的entry，则在这里指定，否则为0
} Elf64_Shdr;

// ELF Section Types, sh_type
#define SHT_NULL 0
#define SHT_PROGBITS 1
#define SHT_SYMTAB 2 // Identifies a symbol table
#define SHT_STRTAB 3 // Identifies a string table.
#define SHT_RELA 4
#define SHT_HASH 5
#define SHT_DYNAMIC 6
#define SHT_NOTE 7
#define SHT_NOBITS 8
#define SHT_REL 9
#define SHT_SHLIB 10
#define SHT_DYNSYM 11 // Identifies a symbol table
#define SHT_INIT_ARRAY 14
#define SHT_FINI_ARRAY 15
#define SHT_PREINIT_ARRAY 16
#define SHT_GROUP 17
#define SHT_SYMTAB_SHNDX 18
#define SHT_LOOS 0x60000000
#define SHT_LOSUNW 0x6fffffef
#define SHT_SUNW_capchain 0x6fffffef
#define SHT_SUNW_capinfo 0x6ffffff0
#define SHT_SUNW_symsort 0x6ffffff1
#define SHT_SUNW_tlssort 0x6ffffff2
#define SHT_SUNW_LDYNSYM 0x6ffffff3 // Identifies a symbol table
#define SHT_SUNW_dof 0x6ffffff4
#define SHT_SUNW_cap 0x6ffffff5
#define SHT_SUNW_SIGNATURE 0x6ffffff6
#define SHT_SUNW_ANNOTATE 0x6ffffff7
#define SHT_SUNW_DEBUGSTR 0x6ffffff8
#define SHT_SUNW_DEBUG 0x6ffffff9
#define SHT_SUNW_move 0x6ffffffa
#define SHT_SUNW_COMDAT 0x6ffffffb
#define SHT_SUNW_syminfo 0x6ffffffc
#define SHT_SUNW_verdef 0x6ffffffd
#define SHT_SUNW_verneed 0x6ffffffe
#define SHT_SUNW_versym 0x6fffffff
#define SHT_HISUNW 0x6fffffff
#define SHT_HIOS 0x6fffffff
#define SHT_LOPROC 0x70000000
#define SHT_SPARC_GOTDATA 0x70000000
#define SHT_AMD64_UNWIND 0x70000001
#define SHT_HIPROC 0x7fffffff
#define SHT_LOUSER 0x80000000
#define SHT_HIUSER 0xffffffff

// ELF Section Attribute Flags
#define SHF_WRITE 0x1     // Identifies a section that should be writable during process execution
#define SHF_ALLOC 0x2     // Identifies a section that occupies memory during process execution
#define SHF_EXECINSTR 0x4 // contains executable machine instructions
#define SHF_MERGE 0x10
#define SHF_STRINGS 0x20
#define SHF_INFO_LINK 0x40  // This section headers sh_info field holds a section header table index
#define SHF_LINK_ORDER 0x80 // This section adds special ordering requirements to the link-editor
#define SHF_OS_NONCONFORMING 0x100
#define SHF_GROUP 0x200
#define SHF_TLS 0x400
#define SHF_MASKOS 0x0ff00000
#define SHF_AMD64_LARGE 0x10000000 // identifies a section that can hold more than 2 Gbyte
#define SHF_ORDERED 0x40000000
#define SHF_EXCLUDE 0x80000000
#define SHF_MASKPROC 0xf0000000

// --> end ==============SHDR=====================

// --> begin ========== symbol table section ======
typedef struct
{
    Elf32_Word st_name;
    Elf32_Addr st_value;
    Elf32_Word st_size;
    unsigned char st_info;
    unsigned char st_other;
    Elf32_Half st_shndx;
} Elf32_Sym;
typedef struct
{
    Elf64_Word st_name;
    unsigned char st_info;
    unsigned char st_other;
    Elf64_Half st_shndx;
    Elf64_Addr st_value;
    Elf64_Xword st_size;
} Elf64_Sym;

// --> end ========== symbol table section ======

// --> begin ========== program header =========
// Ref: https://docs.oracle.com/cd/E23824_01/html/819-0690/chapter6-83432.html#scrolltoc

typedef struct
{
    Elf32_Word p_type;
    Elf32_Off p_offset;
    Elf32_Addr p_vaddr;
    Elf32_Addr p_paddr;
    Elf32_Word p_filesz;
    Elf32_Word p_memsz;
    Elf32_Word p_flags;
    Elf32_Word p_align;
} Elf32_Phdr;

typedef struct
{
    Elf64_Word p_type;
    Elf64_Word p_flags;
    Elf64_Off p_offset;
    Elf64_Addr p_vaddr;
    Elf64_Addr p_paddr;
    Elf64_Xword p_filesz;
    Elf64_Xword p_memsz;
    Elf64_Xword p_align;
} Elf64_Phdr;

// ELF segment types
#define PT_NULL 0
#define PT_LOAD 1    // Specifies a loadable segment
#define PT_DYNAMIC 2 // Specifies dynamic linking information.
#define PT_INTERP 3  // Specifies the location and size of a null-terminated path name to invoke as an interpreter
#define PT_NOTE 4    // Specifies the location and size of auxiliary information
#define PT_SHLIB 5
#define PT_PHDR 6 // Specifies the location and size of the program header table
#define PT_TLS 7  // Specifies a thread-local storage template
/*
PT_LOOS - PT_HIOS
Values in this inclusive range are reserved for OS-specific semantics.
*/
#define PT_LOOS 0x60000000
#define PT_SUNW_UNWIND 0x6464e550
#define PT_SUNW_EH_FRAME 0x6474e550
#define PT_LOSUNW 0x6ffffffa
#define PT_SUNWBSS 0x6ffffffa
#define PT_SUNWSTACK 0x6ffffffb
#define PT_SUNWDTRACE 0x6ffffffc
#define PT_SUNWCAP 0x6ffffffd
#define PT_HISUNW 0x6fffffff
#define PT_HIOS 0x6fffffff
#define PT_LOPROC 0x70000000
#define PT_HIPROC 0x7fffffff

//  ELF Segment Flags
#define PF_X 0x1               // Execute
#define PF_W 0x2               // Write
#define PF_R 0x4               // Read
#define PF_MASKPROC 0xf0000000 // Unspecified


// --> end ========== program header =========

/**
 * @brief 校验是否为ELF文件
 * 
 * @param ehdr 
 */
bool elf_check(void * ehdr);