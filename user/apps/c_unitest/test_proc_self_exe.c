/**
 * @file test_proc_self_exe.c
 * @brief 测试通过 /proc/self/exe 来执行进程
 * 
 * 这个测试用例验证以下内容：
 * 1. 能够通过readlink读取/proc/self/exe
 * 2. 能够通过/proc/self/exe来执行程序
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <errno.h>
#include <sys/wait.h>

#define BUF_SIZE 4096

int main(int argc, char *argv[])
{
    // 如果有参数，说明是被重新执行的
    if (argc > 1 && strcmp(argv[1], "reexec") == 0) {
        printf("[Child] Successfully re-executed via /proc/self/exe\n");
        printf("[Child] My PID: %d\n", getpid());
        return 0;
    }

    printf("[Parent] Testing /proc/self/exe functionality\n");
    printf("[Parent] My PID: %d\n", getpid());

    // 测试1: 使用readlink读取/proc/self/exe
    char exe_path[BUF_SIZE];
    ssize_t len = readlink("/proc/self/exe", exe_path, sizeof(exe_path) - 1);
    if (len == -1) {
        perror("[Parent] readlink(/proc/self/exe) failed");
        return 1;
    }
    exe_path[len] = '\0';
    printf("[Parent] /proc/self/exe -> %s\n", exe_path);

    // 测试2: 尝试通过/proc/self/exe来执行程序
    printf("[Parent] Attempting to execute /proc/self/exe...\n");
    
    pid_t pid = fork();
    if (pid == -1) {
        perror("[Parent] fork failed");
        return 1;
    }

    if (pid == 0) {
        // 子进程：通过/proc/self/exe执行
        char *new_argv[] = {"/proc/self/exe", "reexec", NULL};
        char *new_envp[] = {NULL};
        
        printf("[Child] About to execve(/proc/self/exe, ...)\n");
        execve("/proc/self/exe", new_argv, new_envp);
        
        // 如果执行到这里，说明execve失败了
        perror("[Child] execve(/proc/self/exe) failed");
        printf("[Child] Error code: %d (%s)\n", errno, strerror(errno));
        exit(1);
    } else {
        // 父进程：等待子进程结束
        int status;
        if (waitpid(pid, &status, 0) == -1) {
            perror("[Parent] waitpid failed");
            return 1;
        }

        if (WIFEXITED(status)) {
            int exit_code = WEXITSTATUS(status);
            printf("[Parent] Child exited with code: %d\n", exit_code);
            if (exit_code == 0) {
                printf("[Parent] Test PASSED!\n");
                return 0;
            } else {
                printf("[Parent] Test FAILED - child returned error\n");
                return 1;
            }
        } else {
            printf("[Parent] Child did not exit normally\n");
            return 1;
        }
    }

    return 0;
}
