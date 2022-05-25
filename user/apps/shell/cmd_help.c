#include "cmd_help.h"
#include <libc/stdio.h>
struct help_table_item_t
{
    void (*func)();
};
struct help_table_item_t help_table[] = {
    {shell_help_cd},
};

static const int help_table_num = sizeof(help_table) / sizeof(struct help_table_item_t);

void shell_help()
{
    printf("Help:\n");
    for (int i = 0; i < help_table_num; ++i)
        help_table[i].func();
}

void shell_help_cd()
{
    printf("Example of cd: cd [destination]\n");
}