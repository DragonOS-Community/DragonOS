#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <sched.h>
#include <errno.h>
#include <string.h>

#ifndef CLONE_NEWIPC
#define CLONE_NEWIPC 0x08000000
#endif

int main() {
    printf("Hello World from container test!\n");
    printf("Before unshare: PID = %d\n", getpid());
    
    // 使用 unshare 系统调用创建新的 IPC namespace
    if (unshare(CLONE_NEWIPC) != 0) {
        printf("unshare CLONE_NEWIPC failed: %s\n", strerror(errno));
        return 1;
    }
    
    printf("Success! Created new IPC namespace\n");
    printf("Hello World from new IPC namespace! PID = %d\n", getpid());
    
    return 0;
}