#include "elf.h"
#include <common/unistd.h>
#include <common/glib.h>

/**
 * @brief 校验是否为ELF文件
 *
 * @param ehdr
 */
bool elf_check(void *ehdr)
{
    Elf32_Ehdr *ptr = (Elf32_Ehdr *)ehdr;
    bool flag = ptr->e_ident[EI_MAG0] == ELFMAG0 && ptr->e_ident[EI_MAG1] == ELFMAG1 && ptr->e_ident[EI_MAG2] == ELFMAG2 && ptr->e_ident[EI_MAG3] == ELFMAG3;

    // 标头已经不符合要求
    if (!flag)
        return false;

    // 检验EI_CLASS是否合法
    if (ptr->e_ident[EI_CLASS] == 0 || ptr->e_ident[EI_CLASS] > 2)
        return false;
    
    // 检验EI_DATA是否合法
    if (ptr->e_ident[EI_DATA] == 0 || ptr->e_ident[EI_DATA] > 2)
        return false;
    
    // 检验EI_VERSION是否合法
    if(ptr->e_ident[EI_VERSION]==EV_NONE)
        return false;
    // 是elf文件
    return true;
}

