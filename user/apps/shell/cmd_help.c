#include "cmd_help.h"
#include <stdio.h>
#include <stdlib.h>

struct help_table_item_t
{
    void (*func)();
};
struct help_table_item_t help_table[] = {
    {shell_help_cd},
};

static const int help_table_num = sizeof(help_table) / sizeof(struct help_table_item_t);

int shell_help(int argc, char **argv)
{
    printf("Help:\n");
    for (int i = 0; i < help_table_num; ++i)
        help_table[i].func();

    if (argc > 1)
        free(argv);
    return 0;
}

void shell_help_cd()
{
    printf("Example of cd: cd [destination]\n");
}