#include <fcntl.h>
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

int puts(const char *s)
{
    return put_string(s, COLOR_WHITE, COLOR_BLACK);
}

int putchar(int c)
{
    return printf("%c", (char)c);
}

int fflush(FILE *stream)
{
    return 0;
}

int ferror(FILE *stream)
{
    return 0;
}

int fclose(FILE *stream)
{
    if (stream->fd >= 3)
    {
        int retcval = close(stream);
        free(stream);
        return;
    }
    else
        return 0;
}

FILE *fopen(const char *restrict pathname, const char *restrict mode)
{
    FILE *stream = malloc(sizeof(FILE));
    memset(stream, 0, sizeof(FILE));

    int fd = open(pathname, mode);
    if (fd >= 0)
        stream->fd = fd;
    return stream;
}
