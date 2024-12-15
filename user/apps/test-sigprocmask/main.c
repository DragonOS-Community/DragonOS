#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

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


static int signal_received = 0;

void signal_handler(int signo) {
    if (signo == SIGINT) {
        printf("\nReceived SIGINT (Ctrl+C)\n");
        signal_received = 1;
    }
}

void print_signal_mask(const char *msg, const sigset_t *mask) {
    printf("%s: ", msg);
    for (int signo = 1; signo < NSIG; ++signo) {
        if (sigismember(mask, signo)) {
            printf("%d ", signo);
        }
    }
    printf("\n");
}

// 获取当前屏蔽字的函数
unsigned long get_signal_mask() {
    sigset_t sigset;
    if (sigprocmask(SIG_BLOCK, NULL, &sigset) == -1) {
        perror("sigprocmask");
        return -1; // 返回错误标记
    }

    // 将信号集编码为位掩码
    unsigned long mask = 0;
    for (int i = 1; i < NSIG; i++) {
        if (sigismember(&sigset, i)) {
            mask |= 1UL << (i - 1);
        }
    }
    return mask;
}

int main() {
    sigset_t new_mask, old_mask;
    sigemptyset(&old_mask);

    // 注册 SIGINT 的信号处理函数
    if (signal(SIGINT, signal_handler) == SIG_ERR) {
        perror("signal");
        exit(EXIT_FAILURE);
    }
    printf("Signal handler for SIGINT is registered.\n");
    signal_received = 0;
    kill(getpid(), SIGINT);
    sleep(5);

    TEST_ASSERT(signal_received, 1, "SIGINT was received", "SIGINT was not received");
    signal_received = 0;

    // 初始化新的信号集，并将 SIGINT 添加到其中
    sigemptyset(&new_mask);
    sigaddset(&new_mask, SIGINT);

    // 打印 new_mask 的值
    print_signal_mask("new_mask", &new_mask);

    // 屏蔽 SIGINT
    if (sigprocmask(SIG_BLOCK, &new_mask, &old_mask) < 0) {
        perror("sigprocmask - SIG_BLOCK");
        exit(EXIT_FAILURE);
    }

    // 打印 old_mask 的值
    print_signal_mask("old_mask", &old_mask);

    // 检查 SIGINT 是否被屏蔽
    unsigned long actual_mask = get_signal_mask();
    unsigned long expected_mask = (1UL << (SIGINT - 1));
    TEST_ASSERT(actual_mask,
                expected_mask,
                "Signal mask is as expected",
                "Signal mask mismatch");

    printf("SIGINT is now blocked.\n");
    signal_received = 0;
    // 向当前进程发送 SIGINT
    kill(getpid(), SIGINT);

    // 等待 5 秒，以便测试 SIGINT 是否被屏蔽
    sleep(5);
    TEST_ASSERT(signal_received, 0, "SIGINT was blocked", "SIGINT was not blocked");
    signal_received = 0;
    // 恢复原来的信号屏蔽字
    if (sigprocmask(SIG_SETMASK, &old_mask, &old_mask) < 0) {
        perror("sigprocmask - SIG_SETMASK");
        exit(EXIT_FAILURE);
    }
    print_signal_mask("old_mask returned", &old_mask);

    // 检查 SIGINT 是否被解除屏蔽
    actual_mask = get_signal_mask();
    expected_mask = 0;
    TEST_ASSERT(actual_mask,
                expected_mask,
                "Signal mask is as expected",
                "Signal mask mismatch");

    printf("SIGINT is now unblocked.\n");

    signal_received = 0;
    kill(getpid(), SIGINT);

    // 等待 5 秒，以便测试 SIGINT 是否解除屏蔽
    sleep(5);
    TEST_ASSERT(signal_received, 1, "SIGINT was received", "SIGINT was not received");

    printf("Exiting program.\n");
    return 0;
}
