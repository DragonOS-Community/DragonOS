#include <stdio.h>

FILE* stdin = {0};
FILE* stdout = {0};
FILE* stderr = {0};

void _libc_init()
{
    // 初始化标准流对应的文件描述符
    stdin->fd = 0;
    stdout->fd = 1;
    stderr->fd = 2;
    
}