#include <gtest/gtest.h>

#include <errno.h>
#include <pthread.h>
#include <setjmp.h>
#include <signal.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

#include <atomic>

namespace {

constexpr size_t kPageSize = 4096;
constexpr size_t kNrPages = 64;
constexpr int kNrThreads = 4;
constexpr int kIters = 2000;
constexpr int kMunmapRounds = 16;

thread_local sigjmp_buf g_segv_jmp;
thread_local volatile sig_atomic_t g_segv_active = 0;

void sigsegv_handler(int sig, siginfo_t* si, void* uc) {
    (void)sig;
    (void)si;
    (void)uc;
    if (g_segv_active) {
        siglongjmp(g_segv_jmp, 1);
    }
    _exit(99);
}

int install_segv_handler() {
    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_flags = SA_SIGINFO | SA_NODEFER;
    sa.sa_sigaction = sigsegv_handler;
    sigemptyset(&sa.sa_mask);
    if (sigaction(SIGSEGV, &sa, nullptr) < 0) {
        return -1;
    }
    if (sigaction(SIGBUS, &sa, nullptr) < 0) {
        return -1;
    }
    return 0;
}

struct ThreadCtx {
    volatile uint8_t* base;
    size_t len;
    std::atomic<int>* run;
    std::atomic<int>* ready;
};

void cpu_pause_loop(int rounds) {
    for (volatile int i = 0; i < rounds; ++i) {
#if defined(__x86_64__) || defined(__i386__)
        __asm__ __volatile__("pause" ::: "memory");
#else
        __asm__ __volatile__("" ::: "memory");
#endif
    }
}

void* hammer_writer(void* arg) {
    auto* ctx = static_cast<ThreadCtx*>(arg);
    size_t off = 0;

    if (sigsetjmp(g_segv_jmp, 1) != 0) {
        cpu_pause_loop(1024);
    }
    g_segv_active = 1;

    bool published_ready = false;
    while (ctx->run->load(std::memory_order_acquire)) {
        ctx->base[off] = static_cast<uint8_t>(off);
        if (!published_ready && ctx->ready != nullptr) {
            ctx->ready->fetch_add(1, std::memory_order_release);
            published_ready = true;
        }
        off += 13;
        if (off >= ctx->len) {
            off = 0;
        }
    }

    g_segv_active = 0;
    return nullptr;
}

int start_workers(uint8_t* buf, size_t len, std::atomic<int>* run, pthread_t* threads,
                  ThreadCtx* ctxs, std::atomic<int>* ready = nullptr) {
    int created = 0;
    for (; created < kNrThreads; ++created) {
        ctxs[created].base = buf + created * kPageSize;
        ctxs[created].len = len / kNrPages;
        ctxs[created].run = run;
        ctxs[created].ready = ready;
        if (pthread_create(&threads[created], nullptr, hammer_writer, &ctxs[created]) != 0) {
            fprintf(stderr, "pthread_create failed\n");
            run->store(0, std::memory_order_release);
            for (int i = 0; i < created; ++i) {
                pthread_join(threads[i], nullptr);
            }
            return -1;
        }
    }
    return 0;
}

int wait_for_workers_ready(std::atomic<int>* ready) {
    constexpr int kMaxSpins = 2000000;
    for (int i = 0; i < kMaxSpins; ++i) {
        if (ready->load(std::memory_order_acquire) >= kNrThreads) {
            return 0;
        }
        cpu_pause_loop(64);
    }
    fprintf(stderr, "workers did not become ready\n");
    return -1;
}

void stop_workers(std::atomic<int>* run, pthread_t* threads) {
    run->store(0, std::memory_order_release);
    for (int i = 0; i < kNrThreads; ++i) {
        pthread_join(threads[i], nullptr);
    }
}

int case_mprotect_downgrade() {
    const size_t len = kNrPages * kPageSize;
    auto* buf = static_cast<uint8_t*>(
        mmap(nullptr, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    if (buf == MAP_FAILED) {
        perror("mmap");
        return -1;
    }
    memset(buf, 0, len);

    // Writer state: nonzero = write, 0 = stop.
    std::atomic<int> run{1};
    std::atomic<int> ready{0};
    pthread_t threads[kNrThreads];
    ThreadCtx ctxs[kNrThreads];
    if (start_workers(buf, len, &run, threads, ctxs, &ready) != 0) {
        munmap(buf, len);
        return -1;
    }
    if (wait_for_workers_ready(&ready) != 0) {
        stop_workers(&run, threads);
        munmap(buf, len);
        return -1;
    }

    int rc = 0;

    for (int i = 0; i < kIters; ++i) {
        if (mprotect(buf, len, PROT_READ) < 0) {
            perror("mprotect(R)");
            rc = -1;
            break;
        }

        if (sigsetjmp(g_segv_jmp, 1) == 0) {
            g_segv_active = 1;
            *reinterpret_cast<volatile uint8_t*>(buf) = 0xAA;
            fprintf(stderr, "FAIL: write to RO buf succeeded at iter %d\n", i);
            rc = -1;
            break;
        }
        g_segv_active = 0;

        if (mprotect(buf, len, PROT_READ | PROT_WRITE) < 0) {
            perror("mprotect(RW)");
            rc = -1;
            break;
        }
    }

    g_segv_active = 0;
    stop_workers(&run, threads);
    munmap(buf, len);
    return rc;
}

int case_munmap_while_writing() {
    const size_t len = kNrPages * kPageSize;

    for (volatile int i = 0; i < kMunmapRounds; ++i) {
        auto* buf = static_cast<uint8_t*>(
            mmap(nullptr, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
        if (buf == MAP_FAILED) {
            perror("mmap");
            return -1;
        }
        memset(buf, 0, len);

        std::atomic<int> run{1};
        std::atomic<int> ready{0};
        pthread_t threads[kNrThreads];
        ThreadCtx ctxs[kNrThreads];
        if (start_workers(buf, len, &run, threads, ctxs, &ready) != 0) {
            munmap(buf, len);
            return -1;
        }

        if (wait_for_workers_ready(&ready) != 0) {
            stop_workers(&run, threads);
            munmap(buf, len);
            return -1;
        }
        stop_workers(&run, threads);

        if (munmap(buf, len) < 0) {
            perror("munmap");
            return -1;
        }

        int ok = 0;
        if (sigsetjmp(g_segv_jmp, 1) == 0) {
            g_segv_active = 1;
            *reinterpret_cast<volatile uint8_t*>(buf) = 0xBB;
            fprintf(stderr, "FAIL: write to unmapped buf succeeded (iter %d)\n", i);
        } else {
            ok = 1;
        }
        g_segv_active = 0;

        if (!ok) {
            return -1;
        }
    }

    return 0;
}

// Hammer writer that writes a monotonically increasing per-thread counter
// across the entire buffer.
//
// Used by `case_fork_cow_stale_tlb` to make every CPU in the parent mm cache a
// writable TLB entry for the whole buffer. Unlike `hammer_writer`, this writes
// all pages (not just a per-thread slice) so any writable-TLB leak in the
// parent post-fork will flip bytes anywhere in `buf`.
struct HammerCtx {
    volatile uint8_t* base;
    size_t len;
    std::atomic<int>* run;
    std::atomic<int>* ready;
    uint8_t mark;
};

void* hammer_writer_whole(void* arg) {
    auto* ctx = static_cast<HammerCtx*>(arg);
    size_t off = 0;
    uint8_t v = ctx->mark;

    if (sigsetjmp(g_segv_jmp, 1) != 0) {
        _exit(12);
    }
    g_segv_active = 1;

    if (ctx->ready != nullptr) {
        for (size_t off = 0; off < ctx->len; off += kPageSize) {
            ctx->base[off] = v++;
        }
        ctx->ready->fetch_add(1, std::memory_order_release);
    }

    while (ctx->run->load(std::memory_order_acquire) != 0) {
        ctx->base[off] = v++;
        off += 13;
        if (off >= ctx->len) {
            off = 0;
        }
    }

    g_segv_active = 0;
    return nullptr;
}

// Regression test for the parent-side COW shootdown in `AddressSpace::try_clone`
// and the mm-aware flush in `do_wp_page` (private-anonymous, map_count > 1).
//
// The parent:
//   1. mmaps a private-anon buffer (all-zero after mmap).
//   2. Spawns `kNrThreads` hammer workers that continuously overwrite the whole
//      buffer. On a multi-CPU system this makes every CPU running the parent
//      mm cache a writable TLB entry covering `buf`.
//   3. Forks. `try_clone` writes-protects the parent's PTEs and must
//      synchronously shoot down the parent mm's TLB on *all* active CPUs.
//
// Immediately after fork, the child snapshots `buf` (whatever the hammers
// happened to have left in it) and then repeatedly verifies that no byte has
// changed. Since the child only contains the main thread, any byte flipping
// between the snapshot and a subsequent read must come from a parent CPU
// writing through a stale writable TLB into the physical page that is still
// shared (COW-wise) with the child.
//
// When the fix is in place:
//   - `try_clone` issues a mm-aware `flush_tlb_mm_range` on the parent mm so
//     every CPU invalidates its writable TLB of `buf` before fork returns.
//   - The hammer workers' subsequent writes fault into `do_wp_page` which,
//     in the private-anon map_count > 1 branch, allocates fresh physical
//     pages for the parent mm. The child's physical pages stay pristine.
int case_fork_cow_stale_tlb() {
    const size_t len = kNrPages * kPageSize;
    constexpr size_t kSnapStride = 256;
    constexpr size_t kSnapCount = (kNrPages * kPageSize) / kSnapStride;
    constexpr size_t kSnapMapLen = kPageSize;
    static_assert(kSnapCount <= kSnapMapLen, "snapshot mapping too small");

    auto* buf = static_cast<uint8_t*>(
        mmap(nullptr, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    if (buf == MAP_FAILED) {
        perror("mmap");
        return -1;
    }
    memset(buf, 0, len);

    auto* snap = static_cast<uint8_t*>(
        mmap(nullptr, kSnapMapLen, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    if (snap == MAP_FAILED) {
        perror("mmap snapshot");
        munmap(buf, len);
        return -1;
    }

    int snapshot_pipe[2];
    if (pipe(snapshot_pipe) != 0) {
        perror("pipe");
        munmap(snap, kSnapMapLen);
        munmap(buf, len);
        return -1;
    }
    int verify_pipe[2];
    if (pipe(verify_pipe) != 0) {
        perror("pipe verify");
        close(snapshot_pipe[0]);
        close(snapshot_pipe[1]);
        munmap(snap, kSnapMapLen);
        munmap(buf, len);
        return -1;
    }

    std::atomic<int> run{1};
    std::atomic<int> ready{0};
    pthread_t threads[kNrThreads];
    HammerCtx ctxs[kNrThreads];
    int created = 0;
    for (; created < kNrThreads; ++created) {
        ctxs[created].base = buf;
        ctxs[created].len = len;
        ctxs[created].run = &run;
        ctxs[created].ready = &ready;
        ctxs[created].mark = static_cast<uint8_t>(0x10 + created);
        if (pthread_create(&threads[created], nullptr, hammer_writer_whole, &ctxs[created]) != 0) {
            fprintf(stderr, "pthread_create failed\n");
            run.store(0, std::memory_order_release);
            for (int i = 0; i < created; ++i) {
                pthread_join(threads[i], nullptr);
            }
            close(snapshot_pipe[0]);
            close(snapshot_pipe[1]);
            close(verify_pipe[0]);
            close(verify_pipe[1]);
            munmap(snap, kSnapMapLen);
            munmap(buf, len);
            return -1;
        }
    }

    // Wait until each hammer has touched the full buffer once. Keep them active
    // across fork so the parent mm is still active on remote CPUs when
    // try_clone() write-protects parent PTEs and performs the parent-side
    // shootdown.
    if (wait_for_workers_ready(&ready) != 0) {
        run.store(0, std::memory_order_release);
        for (int i = 0; i < kNrThreads; ++i) {
            pthread_join(threads[i], nullptr);
        }
        close(snapshot_pipe[0]);
        close(snapshot_pipe[1]);
        close(verify_pipe[0]);
        close(verify_pipe[1]);
        munmap(snap, kSnapMapLen);
        munmap(buf, len);
        return -1;
    }

    pid_t pid = fork();
    if (pid < 0) {
        perror("fork");
        run.store(0, std::memory_order_release);
        for (int i = 0; i < kNrThreads; ++i) {
            pthread_join(threads[i], nullptr);
        }
        close(snapshot_pipe[0]);
        close(snapshot_pipe[1]);
        close(verify_pipe[0]);
        close(verify_pipe[1]);
        munmap(snap, kSnapMapLen);
        munmap(buf, len);
        return -1;
    }

    if (pid == 0) {
        close(snapshot_pipe[0]);
        close(verify_pipe[1]);
        volatile uint8_t* child_buf = buf;
        // Child. Snapshot `buf` immediately so we know what the post-fork
        // COW-shared physical pages look like from our point of view. Any
        // subsequent divergence in a sampled byte must be a stale-TLB leak
        // from the parent mm: the child has only one thread and it never
        // writes `buf` itself.
        //
        // Sample every 256 bytes (16 samples/page). `snap` was mmap'ed before
        // creating worker threads so the post-fork child does not enter
        // malloc/free or stdio paths while only one thread survived fork().
        for (size_t i = 0; i < kSnapCount; ++i) {
            snap[i] = child_buf[i * kSnapStride];
        }

        const uint8_t token = 1;
        if (write(snapshot_pipe[1], &token, sizeof(token)) != sizeof(token)) {
            _exit(11);
        }
        close(snapshot_pipe[1]);

        uint8_t start_verify = 0;
        ssize_t nread_verify = 0;
        do {
            nread_verify = read(verify_pipe[0], &start_verify, sizeof(start_verify));
        } while (nread_verify < 0 && errno == EINTR);
        close(verify_pipe[0]);
        if (nread_verify != sizeof(start_verify) || start_verify != 1) {
            _exit(12);
        }

        constexpr int kRounds = 400;
        for (int r = 0; r < kRounds; ++r) {
            for (size_t i = 0; i < kSnapCount; ++i) {
                const uint8_t v = child_buf[i * kSnapStride];
                if (v != snap[i]) {
                    _exit(10);
                }
            }
        }
        _exit(0);
    }

    close(snapshot_pipe[1]);
    close(verify_pipe[0]);
    uint8_t token = 0;
    ssize_t nread = 0;
    do {
        nread = read(snapshot_pipe[0], &token, sizeof(token));
    } while (nread < 0 && errno == EINTR);
    close(snapshot_pipe[0]);
    if (nread != sizeof(token) || token != 1) {
        fprintf(stderr, "child did not publish COW snapshot\n");
        run.store(0, std::memory_order_release);
        for (int i = 0; i < kNrThreads; ++i) {
            pthread_join(threads[i], nullptr);
        }
        close(verify_pipe[1]);
        int status = 0;
        waitpid(pid, &status, 0);
        munmap(snap, kSnapMapLen);
        munmap(buf, len);
        return -1;
    }

    // Keep parent writes active for a bounded post-snapshot window without
    // relying on sched_yield() or nanosleep() progress semantics.
    volatile uint8_t* parent_buf = buf;
    for (int round = 0; round < 128; ++round) {
        for (size_t i = 0; i < kSnapCount; ++i) {
            parent_buf[i * kSnapStride] = static_cast<uint8_t>(round + i);
        }
    }
    const uint8_t start_verify = 1;
    if (write(verify_pipe[1], &start_verify, sizeof(start_verify)) != sizeof(start_verify)) {
        perror("write verify token");
        run.store(0, std::memory_order_release);
        for (int i = 0; i < kNrThreads; ++i) {
            pthread_join(threads[i], nullptr);
        }
        close(verify_pipe[1]);
        int status = 0;
        waitpid(pid, &status, 0);
        munmap(snap, kSnapMapLen);
        munmap(buf, len);
        return -1;
    }
    close(verify_pipe[1]);
    run.store(0, std::memory_order_release);
    for (int i = 0; i < kNrThreads; ++i) {
        pthread_join(threads[i], nullptr);
    }

    int status = 0;
    const pid_t wp = waitpid(pid, &status, 0);
    munmap(snap, kSnapMapLen);
    munmap(buf, len);

    if (wp != pid) {
        perror("waitpid");
        return -1;
    }
    if (!WIFEXITED(status)) {
        fprintf(stderr, "child did not exit normally: status=0x%x\n", status);
        return -1;
    }
    if (WEXITSTATUS(status) != 0) {
        fprintf(stderr, "child exit status=%d\n", WEXITSTATUS(status));
        return -1;
    }
    return 0;
}

TEST(TlbShootdown, MprotectDowngradeInvalidatesRemoteWriters) {
    EXPECT_EQ(0, case_mprotect_downgrade());
}

TEST(TlbShootdown, MunmapAccessFaultsAfterUnmap) {
    EXPECT_EQ(0, case_munmap_while_writing());
}

TEST(TlbShootdown, ForkCowNoStaleTlbLeak) {
    EXPECT_EQ(0, case_fork_cow_stale_tlb());
}

}  // namespace

int main(int argc, char** argv) {
    if (install_segv_handler() < 0) {
        perror("sigaction");
        return 1;
    }

    ::testing::InitGoogleTest(&argc, argv);
    const int rc = RUN_ALL_TESTS();
    _exit(rc == 0 ? 0 : 1);
}
