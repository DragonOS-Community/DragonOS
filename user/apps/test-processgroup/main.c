#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <sys/types.h>
#include <sys/wait.h>

#define TEST_ASSERT(left, right, success_msg, fail_msg)                        \
    do {                                                                       \
        if ((left) == (right)) {                                               \
            printf("[PASS] %s\n", success_msg);                                \
        } else {                                                               \
            printf("[FAIL] %s: Expected 0x%lx, but got 0x%lx\n",               \
                   fail_msg,                                                   \
                   (unsigned long)(right),                                     \
                   (unsigned long)(left));                                     \
        }                                                                      \
    } while (0)



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

        // Assert: PGID 应该等于 PID
        // assert(getpgid(0) == getpid());
        TEST_ASSERT(getpgid(0), getpid(), "Successfully set child1 as processgroup leader", "Child1 PGID check failed");

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

        // Assert: PGID 应该等于 child1 的 PID
        // assert(getpgid(0) == child1);
        TEST_ASSERT(getpgid(0),child1,"Child2 PGID is equal to Child1","Child2 PGID check failed");

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