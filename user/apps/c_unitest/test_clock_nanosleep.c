#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <unistd.h>
#include <time.h>
#include <sys/syscall.h>
#include <errno.h>
#include <signal.h>
#include <string.h>
#include <sys/time.h>
#include <pthread.h>

#ifndef SYS_clock_nanosleep
#define SYS_clock_nanosleep 230 /* x86_64 */
#endif

static inline int do_clock_nanosleep(clockid_t which_clock, int flags, const struct timespec *rqtp, struct timespec *rmtp)
{
    return (int)syscall(SYS_clock_nanosleep, (int)which_clock, (int)flags, rqtp, rmtp);
}

static inline struct timespec ts_add_ms(const struct timespec *a, long ms)
{
    struct timespec r = *a;
    r.tv_nsec += (ms % 1000) * 1000000L;
    r.tv_sec  += ms / 1000;
    if (r.tv_nsec >= 1000000000L) { r.tv_sec += 1; r.tv_nsec -= 1000000000L; }
    return r;
}

// forward declaration
static long ms_since(struct timespec *start);

// test output helpers
static inline void print_run(const char *name) { fprintf(stderr, "[RUN] %s\n", name); }
static inline void print_pass(const char *name) { fprintf(stderr, "[PASS] %s\n", name); }
static inline void print_failed(const char *name) { fprintf(stderr, "[FAILED] %s\n", name); }

static int test_rel_realtime_100ms(void)
{
    fprintf(stderr, "[rel-basic] start\n");
    struct timespec req = { .tv_sec = 0, .tv_nsec = 100 * 1000000L };
    struct timespec t0; clock_gettime(CLOCK_MONOTONIC, &t0);
    int ret = do_clock_nanosleep(CLOCK_REALTIME, 0, &req, NULL);
    long elapsed = ms_since(&t0);
    fprintf(stderr, "[rel-basic] ret=%d errno=%d elapsed=%ldms\n", ret, errno, elapsed);
    if (ret != 0) {
        perror("clock_nanosleep relative (realtime)");
        return -1;
    }
    return 0;
}

static int test_abs_realtime_100ms(void)
{
    struct timespec now;
    if (clock_gettime(CLOCK_REALTIME, &now) != 0) {
        perror("clock_gettime");
        return -1;
    }
    struct timespec abs = ts_add_ms(&now, 100);
    int ret = do_clock_nanosleep(CLOCK_REALTIME, TIMER_ABSTIME, &abs, NULL);
    fprintf(stderr, "[abs-basic] ret=%d errno=%d\n", ret, errno);
    if (ret != 0) {
        perror("clock_nanosleep absolute (realtime)");
        return -1;
    }
    return 0;
}

static volatile sig_atomic_t g_sig_count = 0;
static void sigalrm_handler(int sig)
{
    (void)sig;
    g_sig_count++;
}

static pthread_t g_main_thread;

typedef struct {
    int signo;
    int ms;
} sig_after_args_t;

static void* signal_after_ms_thread(void* p)
{
    sig_after_args_t args = *(sig_after_args_t*)p;
    // free the heap memory allocated in trigger_signal_after_ms
    free(p);
    struct timespec req = { .tv_sec = args.ms / 1000, .tv_nsec = (args.ms % 1000) * 1000000L };
    (void)syscall(SYS_clock_nanosleep, (int)CLOCK_REALTIME, 0, &req, NULL);
    // 定向向主线程发送信号，避免由其他线程消费导致被测线程未被中断
    pthread_kill(g_main_thread, args.signo);
    return NULL;
}

static int trigger_signal_after_ms(int signo, int ms)
{
    pthread_t th;
    sig_after_args_t *args = (sig_after_args_t*)malloc(sizeof(sig_after_args_t));
    if (!args) return -1;
    args->signo = signo;
    args->ms = ms;
    int r = pthread_create(&th, NULL, signal_after_ms_thread, args);
    if (r != 0) return -1;
    // 分离线程，不阻塞主流程
    pthread_detach(th);
    return 0;
}

static long ms_since(struct timespec *start)
{
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    long sec = (long)(now.tv_sec - start->tv_sec);
    long nsec = (long)(now.tv_nsec - start->tv_nsec);
    if (nsec < 0) { sec -= 1; nsec += 1000000000L; }
    return sec * 1000 + nsec / 1000000L;
}

static int test_rel_interrupt_no_restart(void)
{
    struct sigaction sa; memset(&sa, 0, sizeof(sa));
    sa.sa_handler = sigalrm_handler; sa.sa_flags = 0;
    sigemptyset(&sa.sa_mask);
    if (sigaction(SIGALRM, &sa, NULL) != 0) { perror("sigaction"); return -1; }
    g_sig_count = 0;
    trigger_signal_after_ms(SIGALRM, 1000);

    struct timespec req = { .tv_sec = 3, .tv_nsec = 0 };
    struct timespec rem = {0};
    int r = do_clock_nanosleep(CLOCK_REALTIME, 0, &req, &rem);
    fprintf(stderr, "[rel-norestart] r=%d errno=%d rem={%ld,%ld} sigcnt=%d\n", r, errno, rem.tv_sec, rem.tv_nsec, (int)g_sig_count);
    // 再次调用以探知是否是自动重启（若前一次返回0，则这里应该近0的sleep）
    if (r == 0) {
        struct timespec probe = { .tv_sec = 0, .tv_nsec = 10 * 1000000L };
        struct timespec t0; clock_gettime(CLOCK_MONOTONIC, &t0);
        int r2 = do_clock_nanosleep(CLOCK_REALTIME, 0, &probe, NULL);
        long e2 = ms_since(&t0);
        fprintf(stderr, "[rel-norestart-probe] r2=%d errno=%d elapsed=%ldms\n", r2, errno, e2);
    }
    if (r == 0) { fprintf(stderr, "rel no-restart: expected EINTR, got 0\n"); return -1; }
    if (errno != EINTR) { perror("rel no-restart errno"); return -1; }
    if (rem.tv_sec < 0 || rem.tv_nsec < 0 || rem.tv_nsec >= 1000000000L) {
        fprintf(stderr, "rel no-restart: invalid rem {%ld,%ld}\n", rem.tv_sec, rem.tv_nsec);
        return -1;
    }
    return 0;
}

static int test_rel_interrupt_with_restart(void)
{
    struct sigaction sa; memset(&sa, 0, sizeof(sa));
    sa.sa_handler = sigalrm_handler; sa.sa_flags = SA_RESTART;
    sigemptyset(&sa.sa_mask);
    if (sigaction(SIGALRM, &sa, NULL) != 0) { perror("sigaction"); return -1; }
    g_sig_count = 0;
    trigger_signal_after_ms(SIGALRM, 1000);

    struct timespec start; clock_gettime(CLOCK_MONOTONIC, &start);
    struct timespec req = { .tv_sec = 2, .tv_nsec = 0 };
    int r = do_clock_nanosleep(CLOCK_REALTIME, 0, &req, NULL);
    fprintf(stderr, "[rel-restart] r=%d errno=%d\n", r, errno);
    if (r != 0) { perror("rel restart"); return -1; }
    long elapsed = ms_since(&start);
    fprintf(stderr, "[rel-restart] elapsed=%ldms\n", elapsed);
    if (elapsed < 1900) {
        fprintf(stderr, "rel restart: elapsed too small %ldms\n", elapsed);
        return -1;
    }
    return 0;
}

static int test_abs_interrupt_eintr(void)
{
    struct sigaction sa; memset(&sa, 0, sizeof(sa));
    sa.sa_handler = sigalrm_handler; sa.sa_flags = 0;
    sigemptyset(&sa.sa_mask);
    if (sigaction(SIGALRM, &sa, NULL) != 0) { perror("sigaction"); return -1; }
    g_sig_count = 0;

    struct timespec now; clock_gettime(CLOCK_REALTIME, &now);
    struct timespec abs = ts_add_ms(&now, 3000);
    trigger_signal_after_ms(SIGALRM, 1000);
    int r = do_clock_nanosleep(CLOCK_REALTIME, TIMER_ABSTIME, &abs, NULL);
    fprintf(stderr, "[abs-interrupt] r=%d errno=%d sigcnt=%d\n", r, errno, (int)g_sig_count);
    if (r == 0) { fprintf(stderr, "abs interrupt: expected EINTR, got 0\n"); return -1; }
    if (errno != EINTR) { perror("abs interrupt errno"); return -1; }
    return 0;
}

int main(void)
{
    g_main_thread = pthread_self();
    int fails = 0;

    print_run("clock_nanosleep: rel basic 100ms");
    if (test_rel_realtime_100ms() == 0) print_pass("clock_nanosleep: rel basic 100ms");
    else { print_failed("clock_nanosleep: rel basic 100ms"); fails++; }

    print_run("clock_nanosleep: abs basic +100ms");
    if (test_abs_realtime_100ms() == 0) print_pass("clock_nanosleep: abs basic +100ms");
    else { print_failed("clock_nanosleep: abs basic +100ms"); fails++; }

    print_run("clock_nanosleep: rel EINTR no-restart");
    if (test_rel_interrupt_no_restart() == 0) print_pass("clock_nanosleep: rel EINTR no-restart");
    else { print_failed("clock_nanosleep: rel EINTR no-restart"); fails++; }

    print_run("clock_nanosleep: rel SA_RESTART");
    if (test_rel_interrupt_with_restart() == 0) print_pass("clock_nanosleep: rel SA_RESTART");
    else { print_failed("clock_nanosleep: rel SA_RESTART"); fails++; }

    print_run("clock_nanosleep: abs EINTR");
    if (test_abs_interrupt_eintr() == 0) print_pass("clock_nanosleep: abs EINTR");
    else { print_failed("clock_nanosleep: abs EINTR"); fails++; }

    if (fails == 0) return 0; else return 1;
}


