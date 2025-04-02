/**
 * @file kallsyms.c
 * @author longjin (longjin@RinGoTek.cn)
 * @brief 内核栈跟踪
 * @version 0.1
 * @date 2022-06-22
 *
 * @copyright Copyright (c) 2022
 *
 */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/**
 * @brief 判断符号是否需要被输出（只输出text段内的符号）
 *
 */
#define symbol_to_write(vaddr, tv, etv) \
    ((vaddr < tv || vaddr > etv) ? 0 : 1)

/**
 * @brief 使用nm命令提取出来的信息存到这个结构体之中
 *
 */
struct kernel_symbol_entry_t
{
    uint64_t vaddr;
    char type;
    char *symbol;
    int symbol_length;
};

struct kernel_symbol_entry_t *symbol_table;
// 符号表最大能容纳的entry数量
uint64_t table_size = 0;
// 符号表当前的entry数量
uint64_t entry_count = 0;
// 符号表中，text和etext的下标
uint64_t text_vaddr, etext_vaddr;

/**
 * @brief 读取一个符号到entry之中
 *
 * @param filp stdin的文件指针
 * @param entry 待填写的entry
 * @return int 返回码
 */
int read_symbol(FILE *filp, struct kernel_symbol_entry_t *entry)
{
    // 本函数假设nm命令输出的结果中，每行最大512字节
    char str[512] = {0};
    char *s = fgets(str, sizeof(str), filp);
    if (s != str)
    {
        return -1;
    }

    char symbol_name[512] = {0};
    int retval = sscanf(str, "%llx %c %512c", &entry->vaddr, &entry->type, symbol_name);

    // 如果当前行不符合要求
    if (retval != 3 || entry->type != 'T')
    {
        return -1;
    }
    // malloc一块内存，然后把str的内容拷贝进去，接着修改symbol指针
    size_t len = strlen(symbol_name);
    if (len >= 1 && symbol_name[len - 1] == '\n')
    {
        symbol_name[len - 1] = '\0';
        len--;
    }
    // 转义双引号
    for (int i = 0; i < len; i++)
    {
        if (symbol_name[i] == '"')
        {
            char temp[len - i];
            memcpy(temp, symbol_name + i, len - i);
            symbol_name[i] = '\\';
            memcpy(symbol_name + i + 1, temp, len - i);
            i++;
        }
    }
    entry->symbol = strdup(symbol_name);
    entry->symbol_length = len + 1; // +1的原因是.asciz指令会在字符串末尾自动添加结束符\0
    return 0;
}

/**
 * @brief 接收标准输入流的数据，解析nm命令输出的内容
 *
 * @param filp
 */
void read_map(FILE *filp)
{
    // 循环读入数据直到输入流结束
    while (!feof(filp))
    {
        // 给符号表扩容
        if (entry_count >= table_size)
        {
            table_size += 100;
            // 由于使用了realloc，因此符号表原有的内容会被自动的copy过去
            symbol_table = (struct kernel_symbol_entry_t *)realloc(symbol_table, sizeof(struct kernel_symbol_entry_t) * table_size);
        }

        // 若成功读取符号表的内容，则将计数器+1
        if (read_symbol(filp, &symbol_table[entry_count]) == 0)
            ++entry_count;
    }

    // 查找符号表中的text和etext标签
    for (uint64_t i = 0; i < entry_count; ++i)
    {
        if (text_vaddr == 0ULL && strcmp(symbol_table[i].symbol, "_text") == 0)
            text_vaddr = symbol_table[i].vaddr;
        if (etext_vaddr == 0ULL && strcmp(symbol_table[i].symbol, "_etext") == 0)
            etext_vaddr = symbol_table[i].vaddr;
        if (text_vaddr != 0ULL && etext_vaddr != 0ULL)
            break;
    }
}

/**
 * @brief 输出最终的kallsyms汇编代码文件
 * 直接输出到stdout，通过命令行的 > 命令，写入文件
 */
void generate_result()
{
    printf(".section .rodata\n\n");
    printf(".global kallsyms_address\n");
    printf(".align 8\n\n");

    printf("kallsyms_address:\n"); // 地址数组

    uint64_t last_vaddr = 0;
    uint64_t total_syms_to_write = 0; // 真正输出的符号的数量

    // 循环写入地址数组
    for (uint64_t i = 0; i < entry_count; ++i)
    {
        // 判断是否为text段的符号
        if (!symbol_to_write(symbol_table[i].vaddr, text_vaddr, etext_vaddr))
            continue;

        if (symbol_table[i].vaddr == last_vaddr)
            continue;

        // 输出符号地址
        printf("\t.quad\t%#llx\n", symbol_table[i].vaddr);
        ++total_syms_to_write;

        last_vaddr = symbol_table[i].vaddr;
    }

    putchar('\n');

    // 写入符号表的表项数量
    printf(".global kallsyms_num\n");
    printf(".align 8\n");
    printf("kallsyms_num:\n");
    printf("\t.quad\t%lld\n", total_syms_to_write);

    putchar('\n');

    // 循环写入符号名称的下标索引
    printf(".global kallsyms_names_index\n");
    printf(".align 8\n");
    printf("kallsyms_names_index:\n");
    uint64_t position = 0;
    last_vaddr = 0;
    for (uint64_t i = 0; i < entry_count; ++i)
    {
        // 判断是否为text段的符号
        if (!symbol_to_write(symbol_table[i].vaddr, text_vaddr, etext_vaddr))
            continue;

        if (symbol_table[i].vaddr == last_vaddr)
            continue;

        // 输出符号名称的偏移量
        printf("\t.quad\t%lld\n", position);
        position += symbol_table[i].symbol_length;
        last_vaddr = symbol_table[i].vaddr;
    }

    putchar('\n');

    // 输出符号名
    printf(".global kallsyms_names\n");
    printf(".align 8\n");
    printf("kallsyms_names:\n");

    last_vaddr = 0;
    for (uint64_t i = 0; i < entry_count; ++i)
    {
        // 判断是否为text段的符号
        if (!symbol_to_write(symbol_table[i].vaddr, text_vaddr, etext_vaddr))
            continue;

        if (symbol_table[i].vaddr == last_vaddr)
            continue;

        // 输出符号名称
        printf("\t.asciz\t\"%s\"\n", symbol_table[i].symbol);

        last_vaddr = symbol_table[i].vaddr;
    }

    putchar('\n');
}
int main(int argc, char **argv)
{
    read_map(stdin);

    generate_result();
}
