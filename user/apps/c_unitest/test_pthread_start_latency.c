#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <time.h>

#define NR_THREADS 4
#define ROUNDS 1000
#define READY_TIMEOUT_NS 2000000000LL

struct thread_ctx {
    atomic_int *run;
    atomic_int *ready;
};

static void cpu_pause_loop(int rounds) {
    for (volatile int i = 0; i < rounds; ++i) {
#if defined(__x86_64__) || defined(__i386__)
        __asm__ __volatile__("pause" ::: "memory");
#else
        __asm__ __volatile__("" ::: "memory");
#endif
    }
}

static void *worker(void *arg) {
    struct thread_ctx *ctx = (struct thread_ctx *)arg;
    atomic_fetch_add_explicit(ctx->ready, 1, memory_order_release);

    while (atomic_load_explicit(ctx->run, memory_order_acquire) != 0) {
        cpu_pause_loop(64);
    }

    return NULL;
}

static int64_t monotonic_ns(void) {
    struct timespec ts;
    if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0) {
        return -1;
    }
    return (int64_t)ts.tv_sec * 1000000000LL + ts.tv_nsec;
}

static int wait_ready(atomic_int *ready) {
    int64_t start = monotonic_ns();
    if (start < 0) {
        return -1;
    }

    for (;;) {
        if (atomic_load_explicit(ready, memory_order_acquire) >= NR_THREADS) {
            return 0;
        }
        int64_t now = monotonic_ns();
        if (now < 0 || now - start > READY_TIMEOUT_NS) {
            return -1;
        }
        cpu_pause_loop(64);
    }
}

int main(void) {
    for (int round = 0; round < ROUNDS; ++round) {
        atomic_int run = ATOMIC_VAR_INIT(1);
        atomic_int ready = ATOMIC_VAR_INIT(0);
        pthread_t threads[NR_THREADS];
        struct thread_ctx ctx = {
            .run = &run,
            .ready = &ready,
        };

        for (int i = 0; i < NR_THREADS; ++i) {
            int rc = pthread_create(&threads[i], NULL, worker, &ctx);
            if (rc != 0) {
                fprintf(stderr, "pthread_create failed at round=%d thread=%d rc=%d\n", round, i, rc);
                atomic_store_explicit(&run, 0, memory_order_release);
                for (int j = 0; j < i; ++j) {
                    pthread_join(threads[j], NULL);
                }
                return 1;
            }
        }

        if (wait_ready(&ready) != 0) {
            int seen = atomic_load_explicit(&ready, memory_order_acquire);
            fprintf(stderr, "ready timeout at round=%d ready=%d/%d\n", round, seen, NR_THREADS);
            atomic_store_explicit(&run, 0, memory_order_release);
            for (int i = 0; i < NR_THREADS; ++i) {
                pthread_join(threads[i], NULL);
            }
            return 2;
        }

        atomic_store_explicit(&run, 0, memory_order_release);
        for (int i = 0; i < NR_THREADS; ++i) {
            pthread_join(threads[i], NULL);
        }

        if ((round + 1) % 10 == 0) {
            printf("round %d/%d ok\n", round + 1, ROUNDS);
        }
    }

    printf("pthread start latency smoke passed: rounds=%d threads=%d\n", ROUNDS, NR_THREADS);
    return 0;
}
