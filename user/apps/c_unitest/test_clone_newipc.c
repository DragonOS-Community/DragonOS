#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <sys/wait.h>
#include <sched.h>
#include <errno.h>
#include <string.h>

#ifndef CLONE_NEWIPC
#define CLONE_NEWIPC 0x08000000
#endif

int child_func(void *arg) {
    printf("Child process: PID = %d\n", getpid());
    
    // 这里可以添加 IPC 相关的测试代码
    // 比如创建共享内存、信号量等
    
    printf("Child process: IPC namespace test completed\n");
    return 0;
}

int main() {
    printf("Parent process: PID = %d\n", getpid());
    
    // 分配栈空间给子进程
    char *stack = malloc(8192);
    if (!stack) {
        perror("malloc");
        return 1;
    }
    
    // 使用 clone 系统调用创建新的 IPC namespace
    pid_t child_pid = clone(child_func, stack + 8192, 
                           CLONE_NEWIPC | SIGCHLD, NULL);
    
    if (child_pid == -1) {
        perror("clone with CLONE_NEWIPC failed");
        free(stack);
        return 1;
    }
    
    printf("Parent: Created child process with PID %d in new IPC namespace\n", child_pid);
    
    // 等待子进程完成
    int status;
    waitpid(child_pid, &status, 0);
    
    printf("Parent: Child process completed with status %d\n", status);
    
    free(stack);
    return 0;
}