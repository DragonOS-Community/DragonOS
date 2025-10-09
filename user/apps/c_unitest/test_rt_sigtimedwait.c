#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <signal.h>
#include <sys/syscall.h>
#include <errno.h>
#include <string.h>
#include <time.h>
#include <pthread.h>
#include <sys/types.h>

// 兼容性处理：获取 SYS_rt_sigtimedwait 宏
#ifndef SYS_rt_sigtimedwait
#  ifdef __NR_rt_sigtimedwait
#    define SYS_rt_sigtimedwait __NR_rt_sigtimedwait
#  else
#    error "SYS_rt_sigtimedwait 未定义，无法直接进行系统调用测试"
#  endif
#endif

// libc 包装：优先使用 sigtimedwait，保证在不同libc/内核下参数兼容
static int rt_sigtimedwait_libc(const sigset_t *set, siginfo_t *info, const struct timespec *timeout) {
    return sigtimedwait(set, info, timeout);
}

// 原始syscall：用于特定场景（如测试非法sigsetsize）
static int sys_rt_sigtimedwait_raw(const sigset_t *set, siginfo_t *info, const struct timespec *timeout, size_t sigsetsize) {
    return (int)syscall(SYS_rt_sigtimedwait, set, info, timeout, sigsetsize);
}

// 测试统计
static int total_tests = 0;
static int passed_tests = 0;
static int failed_tests = 0;
static char failed_test_names[200][256];
static int failed_test_count = 0;

#define TEST_ASSERT(condition, test_name) do { \
    total_tests++; \
    if (condition) { \
        passed_tests++; \
        printf("PASS - %s\n", test_name); \
    } else { \
        failed_tests++; \
        if (failed_test_count < 200) { \
            snprintf(failed_test_names[failed_test_count], sizeof(failed_test_names[failed_test_count]), "%s", test_name); \
            failed_test_count++; \
        } \
        printf("FAIL - %s (errno=%d: %s)\n", test_name, errno, strerror(errno)); \
    } \
} while(0)

// 工具函数：阻塞/解除阻塞 指定信号
static int block_signal(int sig, sigset_t *oldset) {
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, sig);
    return pthread_sigmask(SIG_BLOCK, &set, oldset);
}

static int unblock_signal(int sig, const sigset_t *oldset) {
    (void)sig; // 未使用，仅保持接口对称
    return pthread_sigmask(SIG_SETMASK, oldset, NULL);
}

// 工具函数：使用rt_sigtimedwait带0超时轮询一次并(如存在)清理信号
static int poll_and_consume_signal_once(int sig) {
    sigset_t set;
    siginfo_t info;
    struct timespec ts = {0, 0};
    sigemptyset(&set);
    sigaddset(&set, sig);
    int r = rt_sigtimedwait_libc(&set, &info, &ts);
    return r; // =sig 表示读到并消费；-1 且 errno=EAGAIN 表示无；其它为异常
}

// 用于线程延迟发送信号
typedef struct {
    pid_t pid;
    int sig;
    int delay_ms;
} sender_args_t;

static void *delayed_kill_sender(void *arg) {
    sender_args_t *a = (sender_args_t *)arg;
    // 确保该线程不接收要发送的信号：阻塞之，让信号成为进程待处理，供主线程 sigtimedwait 消费
    sigset_t set, oldset;
    sigemptyset(&set);
    sigaddset(&set, a->sig);
    pthread_sigmask(SIG_BLOCK, &set, &oldset);

    struct timespec ts;
    ts.tv_sec = a->delay_ms / 1000;
    ts.tv_nsec = (long)(a->delay_ms % 1000) * 1000000L;
    nanosleep(&ts, NULL);
    // 进程定向发送
    kill(a->pid, a->sig);
    // 还原该线程掩码
    pthread_sigmask(SIG_SETMASK, &oldset, NULL);
    return NULL;
}

// 基础功能：阻塞SIGUSR1 -> 自发 -> rt_sigtimedwait应立即返回该信号
static void test_rt_sigtimedwait_basic_self_kill_SIGUSR1() {
    printf("=== 测试: 基础 - 阻塞SIGUSR1后自发并等待 ===\n");

    sigset_t oldset;
    int rc = block_signal(SIGUSR1, &oldset);
    TEST_ASSERT(rc == 0, "阻塞SIGUSR1");

    // 确保没有遗留的待处理SIGUSR1
    while (poll_and_consume_signal_once(SIGUSR1) == SIGUSR1) {}

    // 发送到自身
    pid_t me = getpid();
    rc = kill(me, SIGUSR1);
    TEST_ASSERT(rc == 0, "向自身发送SIGUSR1");

    sigset_t waitset;
    sigemptyset(&waitset);
    sigaddset(&waitset, SIGUSR1);

    siginfo_t info;
    struct timespec timeout = {2, 0}; // 2秒超时，正常应很快返回
    int ret = rt_sigtimedwait_libc(&waitset, &info, &timeout);
    TEST_ASSERT(ret == SIGUSR1, "rt_sigtimedwait 返回SIGUSR1");
    if (ret == SIGUSR1) {
        TEST_ASSERT(info.si_signo == SIGUSR1, "info.si_signo == SIGUSR1");
        // kill() 产生的应为 SI_USER
        TEST_ASSERT(info.si_code == SI_USER, "info.si_code == SI_USER");
        TEST_ASSERT(info.si_pid == me, "info.si_pid 为当前进程");
        TEST_ASSERT(info.si_uid == getuid(), "info.si_uid 为当前用户");
    }

    rc = unblock_signal(SIGUSR1, &oldset);
    TEST_ASSERT(rc == 0, "恢复原有信号屏蔽集");
}

// 超时：等待一个未发送的被阻塞信号，短超时应返回EAGAIN
static void test_rt_sigtimedwait_timeout() {
    printf("\n=== 测试: 超时 - 无信号到达时返回EAGAIN ===\n");

    const int sig = SIGUSR2;
    sigset_t oldset;
    int rc = block_signal(sig, &oldset);
    TEST_ASSERT(rc == 0, "阻塞SIGUSR2");

    // 清理可能的遗留
    while (poll_and_consume_signal_once(sig) == sig) {}

    sigset_t waitset;
    sigemptyset(&waitset);
    sigaddset(&waitset, sig);

    siginfo_t info;
    struct timespec timeout = {0, 100 * 1000 * 1000}; // 100ms
    int ret = rt_sigtimedwait_libc(&waitset, &info, &timeout);
    TEST_ASSERT(ret == -1 && errno == EAGAIN, "无信号到达时超时返回EAGAIN");

    rc = unblock_signal(sig, &oldset);
    TEST_ASSERT(rc == 0, "恢复原有信号屏蔽集");
}

// 非法timespec：tv_nsec越界应返回EINVAL
static void test_rt_sigtimedwait_invalid_timespec() {
    printf("\n=== 测试: 参数校验 - 非法timespec返回EINVAL ===\n");

    const int sig = SIGUSR1;
    sigset_t oldset;
    int rc = block_signal(sig, &oldset);
    TEST_ASSERT(rc == 0, "阻塞SIGUSR1");

    sigset_t waitset;
    sigemptyset(&waitset);
    sigaddset(&waitset, sig);

    siginfo_t info;
    struct timespec bad = { .tv_sec = 0, .tv_nsec = 2000000000L }; // > 1e9-1
    int ret = rt_sigtimedwait_libc(&waitset, &info, &bad);
    TEST_ASSERT(ret == -1 && errno == EINVAL, "tv_nsec越界 -> EINVAL");

    rc = unblock_signal(sig, &oldset);
    TEST_ASSERT(rc == 0, "恢复原有信号屏蔽集");
}

// 延迟发送：NULL超时指针(无限等待) + 后台线程延迟发送实时信号，调用应被唤醒并返回
static void test_rt_sigtimedwait_null_timeout_with_delayed_rt_signal() {
    printf("\n=== 测试: NULL超时 + 延迟发送SIGRTMIN+1 ===\n");

    int rtsig = SIGRTMIN + 1;
    sigset_t oldset;
    int rc = block_signal(rtsig, &oldset);
    TEST_ASSERT(rc == 0, "阻塞SIGRTMIN+1");

    // 线程延迟发送
    pthread_t th;
    sender_args_t args = { .pid = getpid(), .sig = rtsig, .delay_ms = 100 };
    rc = pthread_create(&th, NULL, delayed_kill_sender, &args);
    TEST_ASSERT(rc == 0, "创建延迟发送线程");

    sigset_t waitset;
    sigemptyset(&waitset);
    sigaddset(&waitset, rtsig);
    siginfo_t info;

    // NULL超时：按规范为无限等待，但我们确保100ms内会到信号
    int ret = rt_sigtimedwait_libc(&waitset, &info, NULL);
    TEST_ASSERT(ret == rtsig, "rt_sigtimedwait(NULL) 收到实时信号");
    if (ret == rtsig) {
        TEST_ASSERT(info.si_signo == rtsig, "info.si_signo == 发送的实时信号");
        TEST_ASSERT((info.si_code == SI_USER) || (info.si_code == SI_TKILL) || (info.si_code == SI_QUEUE), "info.si_code 合理");
        TEST_ASSERT(info.si_pid == getpid(), "info.si_pid 为当前进程");
    }

    if (rc == 0) {
        pthread_join(th, NULL);
    }

    rc = unblock_signal(rtsig, &oldset);
    TEST_ASSERT(rc == 0, "恢复原有信号屏蔽集");
}

// 零超时轮询：ts={0,0} 未有信号立刻返回EAGAIN
static void test_rt_sigtimedwait_zero_timeout_poll() {
    printf("\n=== 测试: 轮询模式 - 零超时无信号返回EAGAIN ===\n");

    const int sig = SIGUSR1;
    sigset_t oldset;
    int rc = block_signal(sig, &oldset);
    TEST_ASSERT(rc == 0, "阻塞SIGUSR1");

    // 清空待处理
    while (poll_and_consume_signal_once(sig) == sig) {}

    sigset_t waitset;
    sigemptyset(&waitset);
    sigaddset(&waitset, sig);

    siginfo_t info;
    struct timespec ts = {0, 0};
    int ret = rt_sigtimedwait_libc(&waitset, &info, &ts);
    TEST_ASSERT(ret == -1 && errno == EAGAIN, "零超时无信号 -> EAGAIN");

    rc = unblock_signal(sig, &oldset);
    TEST_ASSERT(rc == 0, "恢复原有信号屏蔽集");
}

// 非法sigsetsize：传0期望返回EINVAL（具体行为依赖内核，这里按Linux常见实现）
static void test_rt_sigtimedwait_invalid_sigsetsize() {
    printf("\n=== 测试: 参数校验 - 非法sigsetsize返回EINVAL ===\n");

    const int sig = SIGUSR1;
    sigset_t oldset;
    int rc = block_signal(sig, &oldset);
    TEST_ASSERT(rc == 0, "阻塞SIGUSR1");

    sigset_t waitset;
    sigemptyset(&waitset);
    sigaddset(&waitset, sig);

    siginfo_t info;
    struct timespec ts = {0, 0};
    int ret = sys_rt_sigtimedwait_raw(&waitset, &info, &ts, 0 /* 非法大小 */);
    TEST_ASSERT(ret == -1 && errno == EINVAL, "sigsetsize=0 -> EINVAL");

    rc = unblock_signal(sig, &oldset);
    TEST_ASSERT(rc == 0, "恢复原有信号屏蔽集");
}

int main() {
    printf("开始 rt_sigtimedwait 系统调用测试\n");
    printf("当前进程 PID=%d\n", getpid());

    test_rt_sigtimedwait_basic_self_kill_SIGUSR1();
    test_rt_sigtimedwait_timeout();
    test_rt_sigtimedwait_invalid_timespec();
    test_rt_sigtimedwait_null_timeout_with_delayed_rt_signal();
    test_rt_sigtimedwait_zero_timeout_poll();
    test_rt_sigtimedwait_invalid_sigsetsize();

    printf("\n=== rt_sigtimedwait 测试完成 ===\n");
    printf("\n=== 测试结果总结 ===\n");
    printf("总测试数: %d\n", total_tests);
    printf("通过: %d\n", passed_tests);
    printf("失败: %d\n", failed_tests);
    printf("成功率: %.1f%%\n", total_tests > 0 ? (float)passed_tests / total_tests * 100 : 0);

    if (failed_tests > 0) {
        printf("\n失败的测试用例:\n");
        for (int i = 0; i < failed_test_count; i++) {
            printf("  - %s\n", failed_test_names[i]);
        }
    } else {
        printf("\n所有测试用例都通过了！\n");
    }

    return failed_tests > 0 ? 1 : 0;
}
