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

int getchar(void)
{
    unsigned int c;
    read(0, &c, 1);
    return c;
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
    int retval = close(stream->fd);
    if (retval)
        return retval;
    if (stream->fd >= 3)
        free(stream);

    return 0;
}

// FIXME:
// 请注意，这个函数的实现，没有遵照posix，行为也与Linux的不一致，请在将来用Rust重构时改变它，以使得它的行为与Linux的一致。
FILE *fopen(const char *restrict pathname, const char *restrict mode)
{
    FILE *stream = malloc(sizeof(FILE));
    memset(stream, 0, sizeof(FILE));
    int o_flags = 0;

    if (strcmp(mode, "r") == 0)
        o_flags = O_RDONLY;
    else if (strcmp(mode, "r+") == 0)
        o_flags = O_RDWR;
    else if (strcmp(mode, "w") == 0)
        o_flags = O_WRONLY;
    else if (strcmp(mode, "w+") == 0)
        o_flags = O_RDWR | O_CREAT;
    else if (strcmp(mode, "a") == 0)
        o_flags = O_APPEND | O_CREAT;
    else if (strcmp(mode, "a+") == 0)
        o_flags = O_APPEND | O_CREAT;

    int fd = open(pathname, o_flags);
    if (fd >= 0)
        stream->fd = fd;
    return stream;
}
