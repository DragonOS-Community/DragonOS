#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <getopt.h>
#include <inttypes.h>
#include <pthread.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <time.h>
#include <unistd.h> 

enum io_mode {
    IO_SEQ_READ = 0,
    IO_SEQ_WRITE,
    IO_RAND_READ,
    IO_RAND_WRITE,
};

struct options {
    const char *path;
    enum io_mode mode;
    size_t bs;
    uint64_t size_bytes;
    int jobs;
    int time_sec;
    int fsync_end;
    uint64_t seed;
};

struct thread_ctx {
    struct options opt;
    int tid;
    uint64_t bytes_target;
    uint64_t file_size;
    pthread_barrier_t *barrier;
    uint64_t bytes_done;
    uint64_t ops_done;
    uint64_t start_ns;
    uint64_t end_ns;
    const char *err_op;
    uint64_t err_off;
    size_t err_len;
    int err;
};

static uint64_t now_ns(void)
{
    struct timespec ts;
    if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0) {
        return 0;
    }
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

static uint64_t xorshift64star(uint64_t *state)
{
    uint64_t x = *state;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    *state = x;
    return x * 2685821657736338717ull;
}

static int parse_mode(const char *s, enum io_mode *out)
{
    if (strcmp(s, "read") == 0) {
        *out = IO_SEQ_READ;
        return 0;
    }
    if (strcmp(s, "write") == 0) {
        *out = IO_SEQ_WRITE;
        return 0;
    }
    if (strcmp(s, "randread") == 0) {
        *out = IO_RAND_READ;
        return 0;
    }
    if (strcmp(s, "randwrite") == 0) {
        *out = IO_RAND_WRITE;
        return 0;
    }
    return -1;
}

static int parse_u64(const char *s, uint64_t *out)
{
    errno = 0;
    char *end = NULL;
    unsigned long long v = strtoull(s, &end, 10);
    if (errno != 0 || end == s || *end != '\0') {
        return -1;
    }
    *out = (uint64_t)v;
    return 0;
}

static int parse_size_bytes(const char *s, uint64_t *out)
{
    if (s == NULL || *s == '\0') {
        return -1;
    }

    errno = 0;
    char *end = NULL;
    unsigned long long v = strtoull(s, &end, 10);
    if (errno != 0 || end == s) {
        return -1;
    }

    uint64_t mul = 1;
    if (*end != '\0') {
        if (end[1] != '\0') {
            return -1;
        }
        switch (*end) {
        case 'k':
        case 'K':
            mul = 1024ull;
            break;
        case 'm':
        case 'M':
            mul = 1024ull * 1024ull;
            break;
        case 'g':
        case 'G':
            mul = 1024ull * 1024ull * 1024ull;
            break;
        default:
            return -1;
        }
    }

    *out = (uint64_t)v * mul;
    return 0;
}

static void print_usage(FILE *out)
{
    fprintf(out,
            "test_ioperf: simplified fio-like IO benchmark\n"
            "\n"
            "Usage:\n"
            "  test_ioperf --file PATH --rw MODE [options]\n"
            "\n"
            "MODE:\n"
            "  read | write | randread | randwrite\n"
            "\n"
            "Options:\n"
            "  -f, --file PATH         target file path\n"
            "  -r, --rw MODE           io pattern\n"
            "  -b, --bs SIZE           block size, e.g. 4K, 128K (default 4K)\n"
            "  -s, --size SIZE         total io bytes (default: 128M for write, file size for read)\n"
            "  -j, --jobs N            threads (default 1)\n"
            "  -t, --time SEC          run time-based (override size loop)\n"
            "      --fsync             fsync at end (default off)\n"
            "      --seed N            random seed (default 1)\n"
            "  -h, --help              show help\n"
            "\n"
            "Examples:\n"
            "  test_ioperf -f /tmp/t.dat -r write --bs 128K --size 512M\n"
            "  test_ioperf -f /tmp/t.dat -r read  --bs 4K --jobs 4\n"
            "  test_ioperf -f /tmp/t.dat -r randread --bs 4K --time 5\n");
}

static int open_file_for_mode(const struct options *opt)
{
    int flags = 0;
    mode_t mode = 0644;

    switch (opt->mode) {
    case IO_SEQ_READ:
    case IO_RAND_READ:
        flags = O_RDONLY;
        break;
    case IO_SEQ_WRITE:
    case IO_RAND_WRITE:
        flags = O_CREAT | O_RDWR;
        break;
    default:
        return -1;
    }

    int fd = open(opt->path, flags, mode);
    if (fd < 0) {
        return -1;
    }
    return fd;
}

static uint64_t get_file_size(int fd)
{
    struct stat st;
    if (fstat(fd, &st) != 0) {
        return 0;
    }
    if (st.st_size < 0) {
        return 0;
    }
    return (uint64_t)st.st_size;
}

static int pread_full(int fd, void *buf, size_t len, off_t off)
{
    size_t done = 0;
    while (done < len) {
        ssize_t r = pread(fd, (char *)buf + done, len - done, off + (off_t)done);
        if (r == 0) {
            errno = EIO;
            return -1;
        }
        if (r < 0) {
            if (errno == EINTR) {
                continue;
            }
            return -1;
        }
        done += (size_t)r;
    }
    return 0;
}

static int pwrite_full(int fd, const void *buf, size_t len, off_t off)
{
    size_t done = 0;
    while (done < len) {
        ssize_t r = pwrite(fd, (const char *)buf + done, len - done, off + (off_t)done);
        if (r < 0) {
            if (errno == EINTR) {
                continue;
            }
            return -1;
        }
        done += (size_t)r;
    }
    return 0;
}

static off_t next_offset_seq(uint64_t *cursor, uint64_t file_size, size_t len)
{
    uint64_t off = *cursor;
    *cursor += (uint64_t)len;
    if (file_size != 0) {
        uint64_t max_off = file_size;
        if (max_off != 0 && *cursor >= max_off) {
            *cursor %= max_off;
        }
        if (off >= max_off) {
            off %= max_off;
        }
    }
    return (off_t)off;
}

static off_t next_offset_rand(uint64_t *rng, uint64_t file_size, size_t bs, size_t len)
{
    uint64_t blocks = 0;
    if (file_size >= bs) {
        blocks = file_size / (uint64_t)bs;
    }
    if (blocks == 0) {
        return 0;
    }

    uint64_t r = xorshift64star(rng);
    uint64_t block = r % blocks;
    uint64_t off = block * (uint64_t)bs;
    if (off + (uint64_t)len > file_size) {
        if (file_size <= (uint64_t)len) {
            return 0;
        }
        off = file_size - (uint64_t)len;
    }
    return (off_t)off;
}

static void *worker(void *arg)
{
    struct thread_ctx *ctx = (struct thread_ctx *)arg;
    ctx->err = 0;
    ctx->err_op = NULL;
    ctx->err_off = 0;
    ctx->err_len = 0;

    int fd = open_file_for_mode(&ctx->opt);
    if (fd < 0) {
        ctx->err = errno ? errno : 1;
        ctx->err_op = "open";
        return NULL;
    }

    void *buf = malloc(ctx->opt.bs);
    if (buf == NULL) {
        ctx->err = ENOMEM;
        ctx->err_op = "malloc";
        close(fd);
        return NULL;
    }

    unsigned char pat = (unsigned char)(0xA5u ^ (unsigned char)ctx->tid);
    memset(buf, pat, ctx->opt.bs);

    int bw = pthread_barrier_wait(ctx->barrier);
    if (bw != 0 && bw != PTHREAD_BARRIER_SERIAL_THREAD) {
        ctx->err = bw;
        ctx->err_op = "barrier_wait";
        free(buf);
        close(fd);
        return NULL;
    }

    uint64_t rng = ctx->opt.seed ^ (0x9e3779b97f4a7c15ull * (uint64_t)(ctx->tid + 1));
    uint64_t cursor = (uint64_t)ctx->tid * ctx->bytes_target;
    uint64_t start = now_ns();
    ctx->start_ns = start;

    uint64_t bytes_done = 0;
    uint64_t ops_done = 0;

    if (ctx->opt.time_sec > 0) {
        uint64_t deadline = start + (uint64_t)ctx->opt.time_sec * 1000000000ull;
        while (true) {
            uint64_t t = now_ns();
            if (t >= deadline) {
                break;
            }

            size_t len = ctx->opt.bs;
            off_t off = 0;
            if (ctx->opt.mode == IO_SEQ_READ || ctx->opt.mode == IO_SEQ_WRITE) {
                off = next_offset_seq(&cursor, ctx->file_size, len);
            } else {
                off = next_offset_rand(&rng, ctx->file_size, ctx->opt.bs, len);
            }

            int rc = 0;
            if (ctx->opt.mode == IO_SEQ_READ || ctx->opt.mode == IO_RAND_READ) {
                rc = pread_full(fd, buf, len, off);
            } else {
                rc = pwrite_full(fd, buf, len, off);
            }
            if (rc != 0) {
                ctx->err = errno ? errno : 1;
                ctx->err_op = (ctx->opt.mode == IO_SEQ_READ || ctx->opt.mode == IO_RAND_READ) ? "pread" : "pwrite";
                ctx->err_off = (uint64_t)off;
                ctx->err_len = len;
                break;
            }
            bytes_done += (uint64_t)len;
            ops_done += 1;
        }
    } else {
        uint64_t target = ctx->bytes_target;
        while (bytes_done < target) {
            uint64_t rem = target - bytes_done;
            size_t len = rem < (uint64_t)ctx->opt.bs ? (size_t)rem : ctx->opt.bs;

            off_t off = 0;
            if (ctx->opt.mode == IO_SEQ_READ || ctx->opt.mode == IO_SEQ_WRITE) {
                off = next_offset_seq(&cursor, ctx->file_size, len);
            } else {
                off = next_offset_rand(&rng, ctx->file_size, ctx->opt.bs, len);
            }

            int rc = 0;
            if (ctx->opt.mode == IO_SEQ_READ || ctx->opt.mode == IO_RAND_READ) {
                rc = pread_full(fd, buf, len, off);
            } else {
                rc = pwrite_full(fd, buf, len, off);
            }
            if (rc != 0) {
                ctx->err = errno ? errno : 1;
                ctx->err_op = (ctx->opt.mode == IO_SEQ_READ || ctx->opt.mode == IO_RAND_READ) ? "pread" : "pwrite";
                ctx->err_off = (uint64_t)off;
                ctx->err_len = len;
                break;
            }

            bytes_done += (uint64_t)len;
            ops_done += 1;
        }
    }

    if (ctx->err == 0 && ctx->opt.fsync_end &&
        (ctx->opt.mode == IO_SEQ_WRITE || ctx->opt.mode == IO_RAND_WRITE)) {
        if (fsync(fd) != 0) {
            ctx->err = errno ? errno : 1;
            ctx->err_op = "fsync";
        }
    }

    ctx->end_ns = now_ns();
    ctx->bytes_done = bytes_done;
    ctx->ops_done = ops_done;

    free(buf);
    close(fd);
    return NULL;
}

static const char *mode_str(enum io_mode m)
{
    switch (m) {
    case IO_SEQ_READ:
        return "read";
    case IO_SEQ_WRITE:
        return "write";
    case IO_RAND_READ:
        return "randread";
    case IO_RAND_WRITE:
        return "randwrite";
    default:
        return "unknown";
    }
}

static uint64_t default_size_for_mode(enum io_mode m)
{
    if (m == IO_SEQ_WRITE || m == IO_RAND_WRITE) {
        return 128ull * 1024ull * 1024ull;
    }
    return 0;
}

static int normalize_options(struct options *opt, uint64_t *file_size_inout, uint64_t *total_io_inout)
{
    if (opt->path == NULL || opt->path[0] == '\0') {
        fprintf(stderr, "missing --file\n");
        return -1;
    }
    if (opt->bs == 0) {
        fprintf(stderr, "invalid --bs\n");
        return -1;
    }
    if (opt->jobs <= 0) {
        fprintf(stderr, "invalid --jobs\n");
        return -1;
    }

    int fd = open_file_for_mode(opt);
    if (fd < 0) {
        fprintf(stderr, "open %s failed: %s\n", opt->path, strerror(errno));
        return -1;
    }

    uint64_t file_size = get_file_size(fd);
    if ((opt->mode == IO_SEQ_READ || opt->mode == IO_RAND_READ) && file_size == 0) {
        fprintf(stderr, "read mode requires non-empty file: %s\n", opt->path);
        close(fd);
        return -1;
    }

    uint64_t req_size = opt->size_bytes;
    if (req_size == 0) {
        uint64_t d = default_size_for_mode(opt->mode);
        req_size = d ? d : file_size;
    }

    if (opt->time_sec > 0) {
        if (opt->mode == IO_SEQ_WRITE || opt->mode == IO_RAND_WRITE) {
            if (req_size == 0) {
                req_size = 128ull * 1024ull * 1024ull;
            }
        }
    }

    if (opt->mode == IO_RAND_READ || opt->mode == IO_RAND_WRITE) {
        uint64_t target_file = req_size;
        if (target_file < opt->bs) {
            fprintf(stderr, "file size must be >= bs for random IO\n");
            close(fd);
            return -1;
        }

        if (opt->mode == IO_RAND_WRITE) {
            if (ftruncate(fd, (off_t)target_file) != 0) {
                fprintf(stderr, "ftruncate failed: %s\n", strerror(errno));
                close(fd);
                return -1;
            }
            file_size = target_file;
        }
    }

    uint64_t total_io = req_size;
    if (opt->mode == IO_SEQ_READ || opt->mode == IO_RAND_READ) {
        if (total_io > file_size) {
            total_io = file_size;
        }
    }

    if (opt->mode == IO_SEQ_WRITE) {
        if (opt->time_sec > 0) {
            if (ftruncate(fd, (off_t)req_size) == 0) {
                file_size = req_size;
            } else {
                file_size = req_size;
            }
        }
    }

    close(fd);

    *file_size_inout = file_size;
    *total_io_inout = total_io;
    return 0;
}

int main(int argc, char **argv)
{
    struct options opt;
    memset(&opt, 0, sizeof(opt));
    opt.mode = IO_SEQ_READ;
    opt.bs = 4096;
    opt.jobs = 1;
    opt.time_sec = 0;
    opt.fsync_end = 0;
    opt.seed = 1;

    static struct option long_opts[] = {
        {"file", required_argument, NULL, 'f'},
        {"rw", required_argument, NULL, 'r'},
        {"bs", required_argument, NULL, 'b'},
        {"size", required_argument, NULL, 's'},
        {"jobs", required_argument, NULL, 'j'},
        {"time", required_argument, NULL, 't'},
        {"fsync", no_argument, NULL, 1000},
        {"seed", required_argument, NULL, 1001},
        {"help", no_argument, NULL, 'h'},
        {0, 0, 0, 0},
    };

    int c = 0;
    while ((c = getopt_long(argc, argv, "f:r:b:s:j:t:h", long_opts, NULL)) != -1) {
        switch (c) {
        case 'f':
            opt.path = optarg;
            break;
        case 'r':
            if (parse_mode(optarg, &opt.mode) != 0) {
                fprintf(stderr, "invalid --rw: %s\n", optarg);
                return 2;
            }
            break;
        case 'b': {
            uint64_t v = 0;
            if (parse_size_bytes(optarg, &v) != 0 || v == 0) {
                fprintf(stderr, "invalid --bs: %s\n", optarg);
                return 2;
            }
            opt.bs = (size_t)v;
            break;
        }
        case 's':
            if (parse_size_bytes(optarg, &opt.size_bytes) != 0) {
                fprintf(stderr, "invalid --size: %s\n", optarg);
                return 2;
            }
            break;
        case 'j': {
            uint64_t v = 0;
            if (parse_u64(optarg, &v) != 0 || v == 0 || v > 1024) {
                fprintf(stderr, "invalid --jobs: %s\n", optarg);
                return 2;
            }
            opt.jobs = (int)v;
            break;
        }
        case 't': {
            uint64_t v = 0;
            if (parse_u64(optarg, &v) != 0 || v > 86400) {
                fprintf(stderr, "invalid --time: %s\n", optarg);
                return 2;
            }
            opt.time_sec = (int)v;
            break;
        }
        case 1000:
            opt.fsync_end = 1;
            break;
        case 1001:
            if (parse_u64(optarg, &opt.seed) != 0) {
                fprintf(stderr, "invalid --seed: %s\n", optarg);
                return 2;
            }
            break;
        case 'h':
            print_usage(stdout);
            return 0;
        default:
            print_usage(stderr);
            return 2;
        }
    }

    uint64_t file_size = 0;
    uint64_t total_io = 0;
    if (normalize_options(&opt, &file_size, &total_io) != 0) {
        print_usage(stderr);
        return 2;
    }

    uint64_t per_job = total_io / (uint64_t)opt.jobs;
    uint64_t rem = total_io % (uint64_t)opt.jobs;

    pthread_t *threads = calloc((size_t)opt.jobs, sizeof(pthread_t));
    struct thread_ctx *ctxs = calloc((size_t)opt.jobs, sizeof(struct thread_ctx));
    if (threads == NULL || ctxs == NULL) {
        fprintf(stderr, "alloc failed\n");
        free(threads);
        free(ctxs);
        return 1;
    }

    pthread_barrier_t barrier;
    if (pthread_barrier_init(&barrier, NULL, (unsigned)opt.jobs) != 0) {
        fprintf(stderr, "pthread_barrier_init failed\n");
        free(threads);
        free(ctxs);
        return 1;
    }

    for (int i = 0; i < opt.jobs; i++) {
        ctxs[i].opt = opt;
        ctxs[i].tid = i;
        ctxs[i].barrier = &barrier;
        ctxs[i].file_size = file_size;
        ctxs[i].bytes_target = per_job + (i < (int)rem ? 1 : 0);
        ctxs[i].bytes_done = 0;
        ctxs[i].ops_done = 0;
        ctxs[i].start_ns = 0;
        ctxs[i].end_ns = 0;
        ctxs[i].err = 0;
        if (pthread_create(&threads[i], NULL, worker, &ctxs[i]) != 0) {
            fprintf(stderr, "pthread_create failed\n");
            return 1;
        }
    }

    uint64_t total_bytes = 0;
    uint64_t total_ops = 0;
    uint64_t min_start = 0;
    uint64_t max_end = 0;
    int first = 1;
    int any_err = 0;
    const char *any_err_op = NULL;
    uint64_t any_err_off = 0;
    size_t any_err_len = 0;

    for (int i = 0; i < opt.jobs; i++) {
        pthread_join(threads[i], NULL);
        if (ctxs[i].err != 0) {
            any_err = ctxs[i].err;
            any_err_op = ctxs[i].err_op;
            any_err_off = ctxs[i].err_off;
            any_err_len = ctxs[i].err_len;
        }
        total_bytes += ctxs[i].bytes_done;
        total_ops += ctxs[i].ops_done;
        if (ctxs[i].start_ns != 0 && ctxs[i].end_ns != 0) {
            if (first) {
                min_start = ctxs[i].start_ns;
                max_end = ctxs[i].end_ns;
                first = 0;
            } else {
                if (ctxs[i].start_ns < min_start) {
                    min_start = ctxs[i].start_ns;
                }
                if (ctxs[i].end_ns > max_end) {
                    max_end = ctxs[i].end_ns;
                }
            }
        }
    }

    pthread_barrier_destroy(&barrier);
    free(threads);
    free(ctxs);

    if (any_err) {
        if (any_err_op != NULL) {
            fprintf(stderr, "io failed: %s off=%" PRIu64 " len=%zu: %s\n", any_err_op, any_err_off, any_err_len,
                    strerror(any_err));
        } else {
            fprintf(stderr, "io failed: %s\n", strerror(any_err));
        }
        return 1;
    }

    double elapsed = 0.0;
    if (max_end > min_start) {
        elapsed = (double)(max_end - min_start) / 1e9;
    }
    if (elapsed <= 0.0) {
        elapsed = 1e-9;
    }

    double iops = (double)total_ops / elapsed;
    double mib = (double)total_bytes / (1024.0 * 1024.0);
    double bw = mib / elapsed;
    double avg_lat_us = total_ops ? ((elapsed * 1e6) / (double)total_ops) : 0.0;

    printf("mode=%s file=%s jobs=%d bs=%zuB\n", mode_str(opt.mode), opt.path, opt.jobs, opt.bs);
    printf("bytes=%" PRIu64 " ops=%" PRIu64 " time=%.6f s\n", total_bytes, total_ops, elapsed);
    printf("bw=%.2f MiB/s iops=%.2f avg_lat=%.2f us\n", bw, iops, avg_lat_us);
    return 0;
}
