#include "cmd_test.h"
#include <libc/stdio.h>
#include <libc/stdlib.h>
#include <libc/string.h>
#include <libc/unistd.h>

int shell_pipe_test(int argc, char **argv)
{
    int ret = -1;
    int fd[2];
    pid_t pid;
    char buf[512] = {0};
    char *msg = "hello world";

    ret = pipe(fd);
    if (-1 == ret) {
        printf("failed to create pipe\n");
        return -1;
    }
    pid = fork();
    if (0 == pid) { 
        // close(fd[0]);
        ret = write(fd[1], msg, strlen(msg)); 
        exit(0);
    } else {          
        // close(fd[1]);
        ret = read(fd[0], buf, sizeof(buf));
        printf("parent read %d bytes data: %s\n", ret, buf);
    }

    return 0;
}