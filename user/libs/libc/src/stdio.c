#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

int fprintf(FILE *restrict stream, const char *restrict format, ...)
{
    const int bufsize = 65536 * 2;
    char *buf = malloc(bufsize);
    memset(buf, 0, bufsize);
    va_list args;

    va_start(args, format);
    vsprintf(buf, format, args);
    va_end(args);

    int len = strlen(buf);
    if (len > bufsize - 1)
    {
        len = bufsize - 1;
        buf[bufsize - 1] = 0;
    }
    write(stream->fd, buf, len);
    free(buf);
}