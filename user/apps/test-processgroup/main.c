#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <sys/types.h>
#include <sys/wait.h>

// 打印进程信息
void print_ids(const char *name) {
    printf("[%s] PID=%d, PPID=%d, PGID=%d, SID=%d\n",
           name,
           getpid(),
           getppid(),
           getpgid(0),  // 获取当前进程的 PGID
           getsid(0));  // 获取当前进程的 SID
}

int main() {
    printf("===== 测试进程组 =====\n");
    print_ids("Parent");

    // 创建第一个子进程
    pid_t child1 = fork();
    if (child1 == 0) {
        // 子进程1：设置自己的进程组
        printf("\n[Child1] 子进程启动...\n");
        print_ids("Child1 (before setpgid)");

        if (setpgid(0, 0) == -1) {  // 将自己的 PGID 设置为自己的 PID
            perror("setpgid failed");
            exit(EXIT_FAILURE);
        }

        print_ids("Child1 (after setpgid)");
        sleep(2);  // 保持运行，便于观察
        exit(EXIT_SUCCESS);
    }

    // 创建第二个子进程
    pid_t child2 = fork();
    if (child2 == 0) {
        // 子进程2：加入第一个子进程的进程组
        printf("\n[Child2] 子进程启动...\n");
        print_ids("Child2 (before setpgid)");

        if (setpgid(0, child1) == -1) {  // 将自己的 PGID 设置为 child1 的 PID
            perror("setpgid failed");
            exit(EXIT_FAILURE);
        }

        print_ids("Child2 (after setpgid)");
        sleep(2);  // 保持运行，便于观察
        exit(EXIT_SUCCESS);
    }

    // 父进程：等待子进程结束
    waitpid(child1, NULL, 0);
    waitpid(child2, NULL, 0);

    printf("\n[Parent] 所有子进程结束后...\n");
    print_ids("Parent");

    return 0;
}