#include <stdio.h>
#include <stdlib.h>

FILE *stdin;
FILE *stdout;
FILE *stderr;

void _libc_init()
{
    // 初始化标准流对应的文件描述符
    stdin = malloc(sizeof(FILE));
    stdout = malloc(sizeof(FILE));
    stderr = malloc(sizeof(FILE));

    stdin->fd = 0;
    stdout->fd = 1;
    stderr->fd = 2;
}