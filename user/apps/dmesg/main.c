#include <stdio.h>
#include <fcntl.h>
#include <unistd.h>
#include <string.h>

int main()
{
    int fd = open("/proc/kmsg", O_RDONLY);
    char buf[1024];

    ssize_t n = 0;
    unsigned int color = 65280;

    while ((n = read(fd, buf, sizeof(buf))) > 0)
    {
        for (int i = 0; i < n; i++)
        {
            char c[2];
            c[0] = buf[i];
            c[1] = '\0';
            syscall(100000, &c[0], color, 0);
            if (buf[i] == ')')
                color = 16744448;
            else if (buf[i] == ']')
                color = 16777215;
            else if (buf[i] == '\n')
                color = 65280;
        }
    }

    close(fd);
}