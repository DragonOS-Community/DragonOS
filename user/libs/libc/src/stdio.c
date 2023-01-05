#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

int fprintf(FILE *restrict stream, const char *restrict format, ...)
{
    char *buf = malloc(65536 * 2);
    memset(buf, 0, 65536 * 2);
    va_list args;

    va_start(args, format);
    vsprintf(buf, format, args);
    va_end(args);
    buf[65536 * 2 - 1] = 0;
    write(stream->fd, buf, strlen(buf));
}