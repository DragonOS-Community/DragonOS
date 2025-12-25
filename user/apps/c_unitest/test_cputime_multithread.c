#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef SYS_clock_nanosleep
#define SYS_clock_nanosleep 230 /* x86_64 */
#endif

static inline int do_clock_nanosleep(clockid_t which_clock, int flags,
                                    const struct timespec *rqtp,
                                    struct timespec *rmtp) {
    return (int)syscall(SYS_clock_nanosleep, (int)which_clock, (int)flags, rqtp, rmtp);
}

static inline uint64_t ts_to_ns(const struct timespec *ts) {
    return (uint64_t)ts->tv_sec * 1000000000ULL + (uint64_t)ts->tv_nsec;
}

static inline struct timespec ns_to_ts(uint64_t ns) {
    struct timespec ts;
    ts.tv_sec = (time_t)(ns / 1000000000ULL);
    ts.tv_nsec = (long)(ns % 1000000000ULL);
    return ts;
}

static uint64_t now_ns(clockid_t clk) {
    struct timespec ts;
    if (clock_gettime(clk, &ts) != 0) {
        perror("clock_gettime");
        exit(2);
    }
    return ts_to_ns(&ts);
}

static void busy_loop_ms(int ms) {
    const uint64_t start = now_ns(CLOCK_MONOTONIC);
    const uint64_t deadline = start + (uint64_t)ms * 1000000ULL;
    volatile uint64_t x = 0;
    while (now_ns(CLOCK_MONOTONIC) < deadline) {
        x = x * 1103515245u + 12345u;
    }
    (void)x;
}

typedef struct {
    int busy_ms;
    uint64_t thread_cpu_delta_ns;
} worker_arg_t;

static void *worker(void *p) {
    worker_arg_t *arg = (worker_arg_t *)p;

    const uint64_t t0 = now_ns(CLOCK_THREAD_CPUTIME_ID);
    busy_loop_ms(arg->busy_ms);
    const uint64_t t1 = now_ns(CLOCK_THREAD_CPUTIME_ID);

    arg->thread_cpu_delta_ns = (t1 >= t0) ? (t1 - t0) : 0;
    return NULL;
}

static int test_process_cputime_sums_threads(void) {
    const int kThreads = 4;
    const int kBusyMs = 300;

    pthread_t th[kThreads];
    worker_arg_t args[kThreads];

    const uint64_t p0 = now_ns(CLOCK_PROCESS_CPUTIME_ID);

    for (int i = 0; i < kThreads; i++) {
        args[i].busy_ms = kBusyMs;
        args[i].thread_cpu_delta_ns = 0;
        int r = pthread_create(&th[i], NULL, worker, &args[i]);
        if (r != 0) {
            errno = r;
            perror("pthread_create");
            return -1;
        }
    }

    for (int i = 0; i < kThreads; i++) {
        pthread_join(th[i], NULL);
    }

    const uint64_t p1 = now_ns(CLOCK_PROCESS_CPUTIME_ID);
    const uint64_t proc_delta = (p1 >= p0) ? (p1 - p0) : 0;

    uint64_t sum_threads = 0;
    uint64_t max_thread = 0;
    for (int i = 0; i < kThreads; i++) {
        sum_threads += args[i].thread_cpu_delta_ns;
        if (args[i].thread_cpu_delta_ns > max_thread) {
            max_thread = args[i].thread_cpu_delta_ns;
        }
    }

    fprintf(stderr,
            "[cputime-sum] proc_delta=%luns sum_threads=%luns max_thread=%luns\n",
            (unsigned long)proc_delta, (unsigned long)sum_threads,
            (unsigned long)max_thread);

    // 最基本的语义校验：
    // - 进程 CPU time 应该推进（>0）
    // - 并且应当 >= 任一线程的线程 CPU time 增量
    if (proc_delta == 0) {
        fprintf(stderr, "proc cputime did not advance\n");
        return -1;
    }
    if (proc_delta < max_thread) {
        fprintf(stderr, "proc cputime less than max thread cputime\n");
        return -1;
    }

    return 0;
}

static int test_clock_nanosleep_process_cputime_abstime(void) {
    // 验证 clock_nanosleep(CLOCK_PROCESS_CPUTIME_ID, TIMER_ABSTIME) 的可达性：
    // 主线程 sleep 到“进程 CPU-time + 200ms”，子线程忙等推进 CPU-time。

    const uint64_t start = now_ns(CLOCK_PROCESS_CPUTIME_ID);
    const uint64_t target = start + 200ULL * 1000000ULL;
    struct timespec abs = ns_to_ts(target);

    pthread_t th;
    worker_arg_t arg;
    arg.busy_ms = 800; // 给足时间推进 CPU-time
    arg.thread_cpu_delta_ns = 0;

    int r = pthread_create(&th, NULL, worker, &arg);
    if (r != 0) {
        errno = r;
        perror("pthread_create");
        return -1;
    }

    errno = 0;
    int ret = do_clock_nanosleep(CLOCK_PROCESS_CPUTIME_ID, TIMER_ABSTIME, &abs, NULL);
    int saved_errno = errno;

    pthread_join(th, NULL);

    const uint64_t end = now_ns(CLOCK_PROCESS_CPUTIME_ID);
    fprintf(stderr,
            "[cputime-sleep] ret=%d errno=%d start=%luns target=%luns end=%luns worker_delta=%luns\n",
            ret, saved_errno, (unsigned long)start, (unsigned long)target,
            (unsigned long)end, (unsigned long)arg.thread_cpu_delta_ns);

    if (ret != 0) {
        errno = saved_errno;
        perror("clock_nanosleep(CLOCK_PROCESS_CPUTIME_ID)");
        return -1;
    }
    if (end < target) {
        fprintf(stderr, "process cputime did not reach target\n");
        return -1;
    }

    return 0;
}

static inline void print_run(const char *name) { fprintf(stderr, "[RUN] %s\n", name); }
static inline void print_pass(const char *name) { fprintf(stderr, "[PASS] %s\n", name); }
static inline void print_failed(const char *name) { fprintf(stderr, "[FAILED] %s\n", name); }

int main(void) {
    int fails = 0;

    print_run("cputime: process sums threads");
    if (test_process_cputime_sums_threads() == 0) {
        print_pass("cputime: process sums threads");
    } else {
        print_failed("cputime: process sums threads");
        fails++;
    }

    print_run("clock_nanosleep: PROCESS_CPUTIME abstime");
    if (test_clock_nanosleep_process_cputime_abstime() == 0) {
        print_pass("clock_nanosleep: PROCESS_CPUTIME abstime");
    } else {
        print_failed("clock_nanosleep: PROCESS_CPUTIME abstime");
        fails++;
    }

    // 回收可能由 pthread 实现带来的子进程（如果 DragonOS pthread 仍是进程模拟）。
    int status;
    for (;;) {
        pid_t reaped = waitpid(-1, &status, WNOHANG);
        if (reaped <= 0) {
            break;
        }
    }

    return fails == 0 ? 0 : 1;
}
