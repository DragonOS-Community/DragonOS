#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <signal.h>
#include <sys/syscall.h>
#include <errno.h>
#include <string.h>
#include <pthread.h>
#include <sys/wait.h>
#include <time.h>

// 系统调用号定义
#define SYS_TKILL 200
#define SYS_TGKILL 234
#define SYS_GETTID 186

// 测试结果统计
static int tests_passed = 0;
static int tests_failed = 0;

// 测试辅助宏
#define TEST_ASSERT(condition, message) do { \
    if (condition) { \
        printf("✓ %s\n", message); \
        tests_passed++; \
    } else { \
        printf("✗ %s\n", message); \
        tests_failed++; \
    } \
} while(0)

// 信号处理函数
static volatile int signal_received = 0;
static volatile int received_signal = 0;

// 线程参数：用于回传可能为独立进程实现的“线程”的 PID/TID
typedef struct {
    int thread_id;
    pid_t pid; // 子“线程”的进程ID（若为独立进程）
    int tid;   // 子“线程”的内核线程ID
} thread_arg_t;

void signal_handler(int sig) {
    signal_received = 1;
    received_signal = sig;
    printf("收到信号: %d\n", sig);
}

// 测试线程函数
void* test_thread(void* arg) {
    thread_arg_t* targ = (thread_arg_t*)arg;
    // 子线程也安装必要的信号处理，确保有序退出
    signal(SIGUSR1, signal_handler);
    signal(SIGTERM, signal_handler);
    targ->pid = getpid();
    targ->tid = syscall(SYS_GETTID);
    printf("测试线程 %d 启动，PID: %d, TID: %d\n", targ->thread_id, targ->pid, targ->tid);
    
    // 等待信号
    while (!signal_received) {
        usleep(10000); // 10ms
    }
    
    printf("测试线程 %d 收到信号 %d，退出\n", targ->thread_id, received_signal);
    return NULL;
}

// 测试1: 基本功能测试
void test_basic_functionality() {
    printf("\n=== 测试1: 基本功能测试 ===\n");
    
    // 安装信号处理器
    signal(SIGUSR1, signal_handler);
    signal_received = 0;
    received_signal = 0;
    
    int tid = syscall(SYS_GETTID);
    printf("当前线程TID: %d\n", tid);
    
    // 测试发送信号给自己
    int result = syscall(SYS_TKILL, tid, SIGUSR1);
    TEST_ASSERT(result == 0, "tkill发送信号给自己应该成功");
    
    // 等待信号处理
    usleep(100000); // 100ms
    TEST_ASSERT(signal_received == 1, "应该收到信号");
    TEST_ASSERT(received_signal == SIGUSR1, "收到的信号应该是SIGUSR1");
}

// 测试2: 参数验证测试
void test_parameter_validation() {
    printf("\n=== 测试2: 参数验证测试 ===\n");
    
    int tid = syscall(SYS_GETTID);
    
    // 测试无效的TID
    int result = syscall(SYS_TKILL, -1, SIGUSR1);
    TEST_ASSERT(result == -1 && errno == EINVAL, "无效TID应该返回EINVAL");
    
    result = syscall(SYS_TKILL, 0, SIGUSR1);
    TEST_ASSERT(result == -1 && errno == EINVAL, "TID为0应该返回EINVAL");
    
    // 测试无效的信号
    result = syscall(SYS_TKILL, tid, -1);
    TEST_ASSERT(result == -1 && errno == EINVAL, "无效信号应该返回EINVAL");
    
    result = syscall(SYS_TKILL, tid, 0);
    TEST_ASSERT(result == 0, "信号为0（探测模式）应该成功");
}

// 测试3: 不存在的线程测试
void test_nonexistent_thread() {
    printf("\n=== 测试3: 不存在的线程测试 ===\n");
    
    // 使用一个不存在的TID
    int result = syscall(SYS_TKILL, 99999, SIGUSR1);
    TEST_ASSERT(result == -1 && errno == ESRCH, "不存在的线程应该返回ESRCH");
}

// 测试4: 多线程测试
void test_multithreaded() {
    printf("\n=== 测试4: 多线程测试 ===\n");
    
    pthread_t thread1, thread2;
    thread_arg_t thread1_arg = { .thread_id = 1, .pid = 0, .tid = 0 };
    thread_arg_t thread2_arg = { .thread_id = 2, .pid = 0, .tid = 0 };
    
    // 重置信号状态
    signal_received = 0;
    received_signal = 0;
    
    // 创建测试线程
    pthread_create(&thread1, NULL, test_thread, &thread1_arg);
    pthread_create(&thread2, NULL, test_thread, &thread2_arg);
    
    // 等待线程启动
    usleep(100000); // 100ms
    
    // 获取线程TID（这里简化处理，实际应该通过其他方式获取）
    int tid = syscall(SYS_GETTID);
    
    // 发送信号给当前线程
    int result = syscall(SYS_TKILL, tid, SIGUSR1);
    TEST_ASSERT(result == 0, "多线程环境下tkill应该工作");
    
    // 等待信号处理
    usleep(100000); // 100ms
    
    // 主动通知子线程/子进程退出，避免遗留被 init 收养
    if (thread1_arg.tid > 0) {
        syscall(SYS_TKILL, thread1_arg.tid, SIGTERM);
    }
    if (thread2_arg.tid > 0) {
        syscall(SYS_TKILL, thread2_arg.tid, SIGTERM);
    }

    // 清理线程
    pthread_join(thread1, NULL);
    pthread_join(thread2, NULL);

    // 由于当前DragonOS下pthread实现可能使用独立进程模拟线程，
    // 这里主动回收任何遗留的“子线程”(子进程)，避免程序退出时被init接管（adopt_childen）。
    int status;
    for (;;) {
        pid_t reaped = waitpid(-1, &status, WNOHANG);
        if (reaped <= 0) {
            break;
        }
    }
}

// 测试5: 探测模式测试
void test_probe_mode() {
    printf("\n=== 测试5: 探测模式测试 ===\n");
    
    int tid = syscall(SYS_GETTID);
    
    // 测试探测模式（信号为0）
    int result = syscall(SYS_TKILL, tid, 0);
    TEST_ASSERT(result == 0, "探测模式应该成功");
    
    // 测试对不存在线程的探测
    result = syscall(SYS_TKILL, 99999, 0);
    TEST_ASSERT(result == -1 && errno == ESRCH, "对不存在线程的探测应该返回ESRCH");
}

// 测试6: 与tgkill的对比测试
void test_tkill_vs_tgkill() {
    printf("\n=== 测试6: tkill vs tgkill 对比测试 ===\n");
    
    int tid = syscall(SYS_GETTID);
    int tgid = getpid();
    
    // 重置信号状态
    signal_received = 0;
    received_signal = 0;
    
    // 使用tkill发送信号
    int tkill_result = syscall(SYS_TKILL, tid, SIGUSR1);
    TEST_ASSERT(tkill_result == 0, "tkill应该成功");
    
    // 等待信号处理
    usleep(100000); // 100ms
    TEST_ASSERT(signal_received == 1, "tkill发送的信号应该被收到");
    
    // 重置信号状态
    signal_received = 0;
    received_signal = 0;
    
    // 使用tgkill发送信号
    // 为SIGUSR2注册handler，避免默认行为打印"User defined signal 2"
    signal(SIGUSR2, signal_handler);
    int tgkill_result = syscall(SYS_TGKILL, tgid, tid, SIGUSR2);
    TEST_ASSERT(tgkill_result == 0, "tgkill应该成功");
    
    // 等待信号处理
    usleep(100000); // 100ms
    TEST_ASSERT(signal_received == 1, "tgkill发送的信号应该被收到");
    TEST_ASSERT(received_signal == SIGUSR2, "收到的信号应该是SIGUSR2");
}

// 测试7: 错误处理测试
void test_error_handling() {
    printf("\n=== 测试7: 错误处理测试 ===\n");
    
    // 测试各种错误情况
    int result;
    
    // 无效TID
    result = syscall(SYS_TKILL, -1, SIGUSR1);
    TEST_ASSERT(result == -1 && errno == EINVAL, "TID为-1应该返回EINVAL");
    
    result = syscall(SYS_TKILL, 0, SIGUSR1);
    TEST_ASSERT(result == -1 && errno == EINVAL, "TID为0应该返回EINVAL");
    
    // 无效信号
    result = syscall(SYS_TKILL, 1, -1);
    TEST_ASSERT(result == -1 && errno == EINVAL, "信号为-1应该返回EINVAL");
    
    // 不存在的线程
    result = syscall(SYS_TKILL, 99999, SIGUSR1);
    TEST_ASSERT(result == -1 && errno == ESRCH, "不存在的线程应该返回ESRCH");
}

// 测试8: 性能测试
void test_performance() {
    printf("\n=== 测试8: 性能测试 ===\n");
    
    int tid = syscall(SYS_GETTID);
    int iterations = 1000;
    
    clock_t start = clock();
    
    for (int i = 0; i < iterations; i++) {
        int result = syscall(SYS_TKILL, tid, 0); // 探测模式，不实际发送信号
        if (result != 0) {
            printf("性能测试中tkill失败: %d\n", result);
            break;
        }
    }
    
    clock_t end = clock();
    double cpu_time_used = ((double)(end - start)) / CLOCKS_PER_SEC;
    
    printf("执行 %d 次tkill调用耗时: %.6f 秒\n", iterations, cpu_time_used);
    printf("平均每次调用耗时: %.6f 秒\n", cpu_time_used / iterations);
    
    TEST_ASSERT(cpu_time_used < 1.0, "性能测试应该在1秒内完成");
}

// 主函数
int main() {
    printf("DragonOS SYS_TKILL 系统调用测试\n");
    printf("================================\n");
    
    // 运行所有测试
    test_basic_functionality();
    test_parameter_validation();
    test_nonexistent_thread();
    test_multithreaded();
    test_probe_mode();
    test_tkill_vs_tgkill();
    test_error_handling();
    test_performance();
    
    // 输出测试结果
    printf("\n================================\n");
    printf("测试结果统计:\n");
    printf("通过: %d\n", tests_passed);
    printf("失败: %d\n", tests_failed);
    printf("总计: %d\n", tests_passed + tests_failed);
    
    if (tests_failed == 0) {
        printf("🎉 所有测试通过！\n");
        return 0;
    } else {
        printf("❌ 有测试失败！\n");
        return 1;
    }
}
