/**
 * test_signalfd_itimer.c — 验证 SIGPROF + signalfd 不产生死锁
 *
 * 背景（Issue #1820）：
 *   timer hardirq → ITIMER_PROF 到期 → send_signal(SIGPROF)
 *   → complete_signal → notify_signalfd_for_pcb
 *   该路径在 hardirq 上下文中执行。如果此时进程上下文正持有
 *   SignalFdInode.state 的 SpinLock（比如在 read/poll 中），
 *   hardirq 重入同一把锁就会自旋死锁。
 *
 * 本测试通过高频 ITIMER_PROF 触发 SIGPROF，同时在循环中
 * 反复 read/poll signalfd，制造 hardirq 与进程上下文的竞争窗口。
 * 如果存在死锁，内核会挂死（测试超时 = 失败）。
 * 能跑完所有迭代即为 PASS。
 *
 * 测试矩阵：
 *   Test 1: signalfd nonblock read  + ITIMER_PROF 高频
 *   Test 2: signalfd blocking read  + ITIMER_PROF（线程发 kill 解除阻塞）
 *   Test 3: signalfd + poll         + ITIMER_PROF 高频
 *   Test 4: signalfd + epoll        + ITIMER_PROF 高频
 */

#include <errno.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/signalfd.h>
#include <sys/time.h>
#include <unistd.h>

/* ---------- 辅助 ---------- */

#define ITERATIONS 50000
#define TIMER_INTERVAL_US 500 /* 500µs，非常激进的 ITIMER_PROF 频率 */

static void fail(const char *msg) {
    perror(msg);
    exit(EXIT_FAILURE);
}

/* 设置 ITIMER_PROF，高频产生 SIGPROF */
static void start_itimer_prof(void) {
    struct itimerval itv;
    memset(&itv, 0, sizeof(itv));
    itv.it_interval.tv_usec = TIMER_INTERVAL_US;
    itv.it_value.tv_usec = TIMER_INTERVAL_US;
    if (setitimer(ITIMER_PROF, &itv, NULL) < 0)
        fail("setitimer(ITIMER_PROF)");
}

static void stop_itimer_prof(void) {
    struct itimerval itv;
    memset(&itv, 0, sizeof(itv));
    setitimer(ITIMER_PROF, &itv, NULL);
}

/* 创建 signalfd 监听 SIGPROF，返回 fd */
static int make_signalfd(int flags) {
    sigset_t mask;
    sigemptyset(&mask);
    sigaddset(&mask, SIGPROF);
    int fd = signalfd(-1, &mask, flags);
    if (fd < 0)
        fail("signalfd");
    return fd;
}

/* 阻塞 SIGPROF（signalfd 要求信号被阻塞） */
static void block_sigprof(void) {
    sigset_t mask;
    sigemptyset(&mask);
    sigaddset(&mask, SIGPROF);
    if (sigprocmask(SIG_BLOCK, &mask, NULL) < 0)
        fail("sigprocmask(SIG_BLOCK, SIGPROF)");
}

/* CPU 密集小循环，消耗 prof-time 让 ITIMER_PROF 有机会 fire */
static void burn_cpu(void) {
    volatile int x = 0;
    for (int j = 0; j < 200; j++)
        x += j;
    (void)x;
}

/* ---------- Test 1: nonblock read ---------- */

static void test_nonblock_read(void) {
    printf("[Test 1] signalfd nonblock read + ITIMER_PROF ... ");
    fflush(stdout);

    int sfd = make_signalfd(SFD_NONBLOCK);
    start_itimer_prof();

    struct signalfd_siginfo si;
    int read_ok = 0;
    for (int i = 0; i < ITERATIONS; i++) {
        ssize_t n = read(sfd, &si, sizeof(si));
        if (n == (ssize_t)sizeof(si))
            read_ok++;
        burn_cpu();
    }

    stop_itimer_prof();
    close(sfd);
    printf("PASS (read_ok=%d/%d)\n", read_ok, ITERATIONS);
}

/* ---------- Test 2: blocking read ---------- */

struct blocking_ctx {
    int sfd;
    int ready; /* 简单栅栏 */
};

static void *blocking_reader(void *arg) {
    struct blocking_ctx *ctx = arg;
    struct signalfd_siginfo si;

    __atomic_store_n(&ctx->ready, 1, __ATOMIC_RELEASE);

    /* 阻塞 read：等待 SIGPROF 到达 */
    ssize_t n = read(ctx->sfd, &si, sizeof(si));
    if (n != (ssize_t)sizeof(si)) {
        /* EINTR 也可以接受 */
        if (errno != EINTR) {
            perror("blocking read");
            return (void *)1;
        }
    }
    return NULL;
}

static void test_blocking_read(void) {
    printf("[Test 2] signalfd blocking read + ITIMER_PROF ... ");
    fflush(stdout);

    int sfd = make_signalfd(0); /* 阻塞模式 */

    struct blocking_ctx ctx = {.sfd = sfd, .ready = 0};
    pthread_t tid;
    if (pthread_create(&tid, NULL, blocking_reader, &ctx) != 0)
        fail("pthread_create");

    /* 等读线程就位 */
    while (!__atomic_load_n(&ctx.ready, __ATOMIC_ACQUIRE))
        usleep(100);
    usleep(1000); /* 让读线程真正进入 read 阻塞 */

    /* 启动 ITIMER_PROF，SIGPROF 会唤醒 signalfd read */
    start_itimer_prof();
    burn_cpu(); /* 确保 timer 有 CPU time 可计量 */
    usleep(50000); /* 50ms，给 ITIMER_PROF 充分触发时间 */

    /* 如果 read 还没返回，发一个 SIGPROF 确保唤醒 */
    kill(getpid(), SIGPROF);

    void *ret;
    pthread_join(tid, &ret);
    stop_itimer_prof();
    close(sfd);

    if (ret != NULL) {
        printf("FAIL\n");
        exit(EXIT_FAILURE);
    }
    printf("PASS\n");
}

/* ---------- Test 3: poll ---------- */

static void test_poll(void) {
    printf("[Test 3] signalfd + poll + ITIMER_PROF ... ");
    fflush(stdout);

    int sfd = make_signalfd(SFD_NONBLOCK);
    start_itimer_prof();

    int poll_ready = 0;
    for (int i = 0; i < ITERATIONS; i++) {
        struct pollfd pfd = {.fd = sfd, .events = POLLIN};
        int r = poll(&pfd, 1, 0); /* timeout=0，立即返回 */
        if (r > 0 && (pfd.revents & POLLIN)) {
            poll_ready++;
            /* 消费掉信号 */
            struct signalfd_siginfo si;
            read(sfd, &si, sizeof(si));
        }
        burn_cpu();
    }

    stop_itimer_prof();
    close(sfd);
    printf("PASS (poll_ready=%d/%d)\n", poll_ready, ITERATIONS);
}

/* ---------- Test 4: epoll ---------- */

static void test_epoll(void) {
    printf("[Test 4] signalfd + epoll + ITIMER_PROF ... ");
    fflush(stdout);

    int sfd = make_signalfd(SFD_NONBLOCK);
    int efd = epoll_create1(0);
    if (efd < 0)
        fail("epoll_create1");

    struct epoll_event ev;
    ev.events = EPOLLIN;
    ev.data.fd = sfd;
    if (epoll_ctl(efd, EPOLL_CTL_ADD, sfd, &ev) < 0)
        fail("epoll_ctl ADD signalfd");

    start_itimer_prof();

    int epoll_ready = 0;
    struct epoll_event revents[4];
    for (int i = 0; i < ITERATIONS; i++) {
        int n = epoll_wait(efd, revents, 4, 0); /* timeout=0 */
        if (n > 0) {
            epoll_ready++;
            struct signalfd_siginfo si;
            read(sfd, &si, sizeof(si));
        }
        burn_cpu();
    }

    stop_itimer_prof();
    close(efd);
    close(sfd);
    printf("PASS (epoll_ready=%d/%d)\n", epoll_ready, ITERATIONS);
}

/* ---------- Test 5: 多 signalfd 并发 ---------- */

struct mt_ctx {
    int id;
    int sfd;
    int ok;
};

static void *mt_worker(void *arg) {
    struct mt_ctx *ctx = arg;
    struct signalfd_siginfo si;
    int count = 0;
    for (int i = 0; i < ITERATIONS / 4; i++) {
        ssize_t n = read(ctx->sfd, &si, sizeof(si));
        if (n == (ssize_t)sizeof(si))
            count++;
        burn_cpu();
    }
    ctx->ok = count;
    return NULL;
}

static void test_multithread(void) {
    printf("[Test 5] multi-thread signalfd + ITIMER_PROF ... ");
    fflush(stdout);

    /* 每个线程各自创建独立的 signalfd */
    const int NTHREADS = 4;
    pthread_t tids[4];
    struct mt_ctx ctxs[4];

    for (int i = 0; i < NTHREADS; i++) {
        ctxs[i].id = i;
        ctxs[i].sfd = make_signalfd(SFD_NONBLOCK);
        ctxs[i].ok = 0;
    }

    start_itimer_prof();

    for (int i = 0; i < NTHREADS; i++) {
        if (pthread_create(&tids[i], NULL, mt_worker, &ctxs[i]) != 0)
            fail("pthread_create mt");
    }

    for (int i = 0; i < NTHREADS; i++)
        pthread_join(tids[i], NULL);

    stop_itimer_prof();

    int total = 0;
    for (int i = 0; i < NTHREADS; i++) {
        total += ctxs[i].ok;
        close(ctxs[i].sfd);
    }

    printf("PASS (total_read=%d)\n", total);
}

/* ---------- main ---------- */

int main(void) {
    printf("=== test_signalfd_itimer (Issue #1820 deadlock verification) ===\n");
    printf("ITERATIONS=%d  TIMER_INTERVAL=%dµs\n\n", ITERATIONS, TIMER_INTERVAL_US);

    block_sigprof();

    test_nonblock_read(); /* Test 1 */
    test_blocking_read(); /* Test 2 */
    test_poll();          /* Test 3 */
    test_epoll();         /* Test 4 */
    test_multithread();   /* Test 5 */

    printf("\n=== ALL TESTS PASSED (no deadlock) ===\n");
    return 0;
}
