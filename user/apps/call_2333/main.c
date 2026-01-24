#define _GNU_SOURCE
#include <stdio.h>
#include <unistd.h>
#include <sys/syscall.h>

int main(void)
{
    printf("Hello, world!\n");

    long ret = syscall(2333);   
    // call custom syscall number 2333
    printf("syscall(2333) returned: %ld\n", ret);

    return 0;
}