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

#define TEST_CONDITION(condition, success_msg, fail_msg)                      \
    do {                                                                      \
        if (condition) {                                                      \
            printf("[PASS] %s\n", success_msg);                               \
        } else {                                                              \
            printf("[FAIL] %s\n", fail_msg);                                  \
        }                                                                     \
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
    printf("===== 测试 getsid =====\n");
    print_ids("Parent");

    pid_t child = fork();
    if (child == 0) {
        // 子进程
        printf("\n[Child] 子进程启动...\n");
        print_ids("Child (before setsid)");

        // 创建新会话
        pid_t newsid = setsid();
        if (newsid == -1) {
            perror("setsid failed");
            exit(EXIT_FAILURE);
        }

        printf("[Child] 创建新会话成功，新 SID = %d\n", newsid);
        print_ids("Child (after setsid)");

        TEST_ASSERT(newsid, getpid(), "New sid equal to child pid", "failed to set new sid");
        TEST_ASSERT(getsid(0), getpid(), "Child sid equal to child pid", "failed to set new sid");
        TEST_ASSERT(getpgid(0), getpid(), "Child pgid equal to child pid", "failed to set new sid");

        exit(EXIT_SUCCESS);
    } else if (child > 0) {
        // 父进程
        waitpid(child, NULL, 0);  // 等待子进程结束
        printf("\n[Parent] 子进程结束后...\n");
        print_ids("Parent");

        TEST_CONDITION(getsid(0)!=child, "Parent sid unchanged", "Parent sid changed");
        TEST_CONDITION(getpgid(0)!=child, "Parent pgid unchanged", "Parent pgid changed");
    } else {
        perror("fork failed");
        exit(EXIT_FAILURE);
    }

    return 0;
}