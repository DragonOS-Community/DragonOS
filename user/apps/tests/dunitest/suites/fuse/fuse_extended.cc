#include <gtest/gtest.h>

#include <signal.h>
#include <sched.h>
#include <setjmp.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <sys/xattr.h>
#include <time.h>

#include "fuse_gtest_common.h"

#ifndef MNT_DETACH
#define MNT_DETACH 2
#endif

static sigjmp_buf g_fuse_sigbus_jmp;
static volatile sig_atomic_t g_fuse_sigbus_seen = 0;
static sigjmp_buf g_fuse_sigsegv_jmp;
static volatile sig_atomic_t g_fuse_sigsegv_seen = 0;

struct fuse_fsync_worker_args {
    int fd;
    volatile int done;
    int result;
    int saved_errno;
};

static void *fuse_fsync_worker(void *opaque) {
    struct fuse_fsync_worker_args *args = (struct fuse_fsync_worker_args *)opaque;
    args->result = fsync(args->fd);
    args->saved_errno = errno;
    __sync_synchronize();
    args->done = 1;
    return NULL;
}

struct fuse_mmap_single_write_args {
    volatile char *address;
    char value;
    volatile int started;
    volatile int done;
};

static void *fuse_mmap_single_write_worker(void *opaque) {
    struct fuse_mmap_single_write_args *args =
        (struct fuse_mmap_single_write_args *)opaque;
    args->started = 1;
    __sync_synchronize();
    *args->address = args->value;
    __sync_synchronize();
    args->done = 1;
    return NULL;
}

static int fuse_monotonic_ms(uint64_t *value) {
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0 || now.tv_sec < 0 || now.tv_nsec < 0) {
        return -1;
    }
    *value = (uint64_t)now.tv_sec * 1000u + (uint64_t)now.tv_nsec / 1000000u;
    return 0;
}

static int fuse_wait_flag(volatile int *flag, unsigned int timeout_ms) {
    uint64_t started_ms = 0;
    uint64_t now_ms = 0;
    if (fuse_monotonic_ms(&started_ms) != 0) {
        return -1;
    }
    while (1) {
        if (*flag) {
            return 0;
        }
        if (fuse_monotonic_ms(&now_ms) != 0 || now_ms - started_ms >= timeout_ms) {
            break;
        }
        usleep(1000);
    }
    return -1;
}

static int fuse_parse_u64_counter(const char *report, const char *field, uint64_t *value) {
    size_t field_len = strlen(field);
    const char *line = report;
    while (line && *line) {
        if (strncmp(line, field, field_len) == 0 && line[field_len] == ' ') {
            char *end = NULL;
            errno = 0;
            unsigned long long parsed = strtoull(line + field_len + 1, &end, 10);
            if (errno == 0 && end != line + field_len + 1) {
                *value = (uint64_t)parsed;
                return 0;
            }
            break;
        }
        line = strchr(line, '\n');
        if (line) {
            ++line;
        }
    }
    return -1;
}

static char *fuse_read_stats_report(const char *path) {
    char *report = (char *)malloc(32768);
    if (!report) {
        return NULL;
    }
    int fd = open(path, O_RDONLY);
    if (fd < 0) {
        free(report);
        return NULL;
    }
    size_t used = 0;
    while (used + 1 < 32768) {
        ssize_t n = read(fd, report + used, 32767 - used);
        if (n < 0) {
            close(fd);
            free(report);
            return NULL;
        }
        if (n == 0) {
            break;
        }
        used += (size_t)n;
    }
    close(fd);
    report[used] = '\0';
    return report;
}

static int fuse_read_u64_counter(const char *path, const char *field, uint64_t *value) {
    char *report = fuse_read_stats_report(path);
    if (!report) {
        return -1;
    }
    int result = fuse_parse_u64_counter(report, field, value);
    free(report);
    return result;
}

static int fuse_wait_counter_increase(const char *path, const char *field, uint64_t before,
                                      uint64_t *after, unsigned int timeout_ms) {
    uint64_t started_ms = 0;
    uint64_t now_ms = 0;
    if (fuse_monotonic_ms(&started_ms) != 0) {
        return -1;
    }
    while (1) {
        if (fuse_read_u64_counter(path, field, after) == 0 && *after > before) {
            return 0;
        }
        if (fuse_monotonic_ms(&now_ms) != 0 || now_ms - started_ms >= timeout_ms) {
            break;
        }
        usleep(1000);
    }
    return -1;
}

struct p4_readdir_daemon_args {
    struct fuse_daemon_args common;
    volatile uint32_t lookup_count;
    volatile uint32_t getattr_count;
    volatile uint32_t forget_count;
    volatile uint32_t readdir_count;
    volatile uint32_t readdirplus_count;
    volatile uint32_t dir_trace_count;
    uint32_t dir_opcode_trace[16];
    uint64_t dir_offset_trace[16];
    int one_entry_per_reply;
};

struct p4_linux_dirent64 {
    uint64_t d_ino;
    int64_t d_off;
    unsigned short d_reclen;
    unsigned char d_type;
    char d_name[1];
};

struct p4_mock_dir_entry {
    const char *name;
    uint64_t nodeid;
    uint64_t ino;
    uint32_t type;
    uint64_t next_cookie;
};

static const struct p4_mock_dir_entry p4_mock_dir_entries[] = {
    {".", 1, 1, DT_DIR, 11},
    {"..", 1, 1, DT_DIR, 29},
    {"hello.txt", 2, 2, DT_REG, 101},
    {"alpha.txt", 3, 3, DT_REG, 4099},
    {"beta.txt", 4, 4, DT_REG, 65537},
};

struct p4_shared_readdir_args {
    int fd;
    volatile int *ready;
    volatile int *go;
    pthread_mutex_t *lock;
    unsigned int *name_counts;
    volatile int error;
};

struct p4_shared_seek_args {
    int fd;
    volatile int *go;
    volatile int *stop;
    volatile int error;
};

static int p4_mock_entry_index(const char *name) {
    for (size_t i = 0; i < sizeof(p4_mock_dir_entries) / sizeof(p4_mock_dir_entries[0]); i++) {
        if (strcmp(name, p4_mock_dir_entries[i].name) == 0) {
            return (int)i;
        }
    }
    return -1;
}

static void *p4_shared_readdir_thread(void *opaque) {
    struct p4_shared_readdir_args *args = (struct p4_shared_readdir_args *)opaque;
    __sync_fetch_and_add(args->ready, 1);
    while (!*args->go) {
        sched_yield();
    }
    for (;;) {
        unsigned char buf[32];
        ssize_t n = syscall(SYS_getdents64, args->fd, buf, sizeof(buf));
        if (n < 0) {
            args->error = errno;
            return NULL;
        }
        if (n == 0) {
            return NULL;
        }
        if ((size_t)n < offsetof(struct p4_linux_dirent64, d_name)) {
            args->error = EIO;
            return NULL;
        }
        struct p4_linux_dirent64 *entry = (struct p4_linux_dirent64 *)buf;
        int index = p4_mock_entry_index(entry->d_name);
        if (index < 0 || entry->d_reclen != (unsigned short)n ||
            entry->d_off != (int64_t)p4_mock_dir_entries[index].next_cookie ||
            entry->d_ino != p4_mock_dir_entries[index].ino ||
            entry->d_type != p4_mock_dir_entries[index].type) {
            args->error = EIO;
            return NULL;
        }
        pthread_mutex_lock(args->lock);
        args->name_counts[index]++;
        pthread_mutex_unlock(args->lock);
    }
}

static void *p4_shared_seek_thread(void *opaque) {
    struct p4_shared_seek_args *args = (struct p4_shared_seek_args *)opaque;
    while (!*args->go) {
        sched_yield();
    }
    while (!*args->stop) {
        if (lseek(args->fd, 0, SEEK_CUR) < 0) {
            args->error = errno;
            return NULL;
        }
    }
    return NULL;
}

static void p4_add_mock_file(struct simplefs *fs, const char *name) {
    struct simplefs_node *node = simplefs_alloc(fs);
    if (!node) {
        return;
    }
    node->parent = 1;
    node->mode = S_IFREG | 0644;
    node->is_dir = 0;
    strncpy(node->name, name, sizeof(node->name) - 1);
    node->name[sizeof(node->name) - 1] = '\0';
}

static int p4_handle_readdir(struct p4_readdir_daemon_args *args,
                             const struct fuse_in_header *header, const unsigned char *payload,
                             size_t payload_len) {
    if (payload_len < sizeof(struct fuse_read_in)) {
        return -1;
    }
    const struct fuse_read_in *in = (const struct fuse_read_in *)payload;
    const bool plus = header->opcode == FUSE_READDIRPLUS;
    size_t index = 0;
    if (in->offset != 0) {
        bool found = false;
        for (size_t i = 0; i < sizeof(p4_mock_dir_entries) / sizeof(p4_mock_dir_entries[0]); i++) {
            if (p4_mock_dir_entries[i].next_cookie == in->offset) {
                index = i + 1;
                found = true;
                break;
            }
        }
        if (!found) {
            return fuse_write_reply(args->common.fd, header->unique, -EINVAL, NULL, 0);
        }
    }

    unsigned char outbuf[1024];
    memset(outbuf, 0, sizeof(outbuf));
    size_t outlen = 0;
    size_t output_limit = in->size < sizeof(outbuf) ? in->size : sizeof(outbuf);
    for (; index < sizeof(p4_mock_dir_entries) / sizeof(p4_mock_dir_entries[0]); index++) {
        const struct p4_mock_dir_entry *entry = &p4_mock_dir_entries[index];
        size_t name_len = strlen(entry->name);
        size_t record_len =
            plus ? fuse_direntplus_rec_len(name_len) : fuse_dirent_rec_len(name_len);
        if (outlen + record_len > output_limit) {
            break;
        }
        if (plus) {
            struct fuse_direntplus dirent;
            memset(&dirent, 0, sizeof(dirent));
            struct simplefs_node *node = simplefs_find_node(&args->common.fs, entry->nodeid);
            dirent.entry_out.nodeid = entry->nodeid;
            dirent.entry_out.generation = node ? node->generation : 1;
            dirent.entry_out.entry_valid = args->common.entry_valid_sec;
            dirent.entry_out.attr_valid = args->common.attr_valid_sec;
            if (node) {
                simplefs_fill_attr(node, &dirent.entry_out.attr);
            }
            dirent.dirent.ino = entry->ino;
            dirent.dirent.off = entry->next_cookie;
            dirent.dirent.namelen = (uint32_t)name_len;
            dirent.dirent.type = entry->type;
            memcpy(outbuf + outlen, &dirent, sizeof(dirent));
            memcpy(outbuf + outlen + sizeof(dirent), entry->name, name_len);
        } else {
            struct fuse_dirent dirent;
            memset(&dirent, 0, sizeof(dirent));
            dirent.ino = entry->ino;
            dirent.off = entry->next_cookie;
            dirent.namelen = (uint32_t)name_len;
            dirent.type = entry->type;
            memcpy(outbuf + outlen, &dirent, sizeof(dirent));
            memcpy(outbuf + outlen + sizeof(dirent), entry->name, name_len);
        }
        outlen += record_len;
        if (args->one_entry_per_reply) {
            break;
        }
    }
    return fuse_write_reply(args->common.fd, header->unique, 0, outbuf, outlen);
}

static void *p4_readdir_daemon_thread(void *opaque) {
    struct p4_readdir_daemon_args *args = (struct p4_readdir_daemon_args *)opaque;
    unsigned char *buf = (unsigned char *)malloc(FUSE_TEST_BUF_SIZE);
    if (!buf) {
        return NULL;
    }
    simplefs_init(&args->common.fs);
    p4_add_mock_file(&args->common.fs, "alpha.txt");
    p4_add_mock_file(&args->common.fs, "beta.txt");

    while (!*args->common.stop) {
        ssize_t n = read(args->common.fd, buf, FUSE_TEST_BUF_SIZE);
        if (n < 0) {
            if (errno == EINTR) {
                continue;
            }
            if (fuse_daemon_read_should_stop(errno)) {
                break;
            }
            continue;
        }
        if (n == 0) {
            break;
        }
        const struct fuse_in_header *header = (const struct fuse_in_header *)buf;
        if ((size_t)n != header->len || (size_t)n < sizeof(*header)) {
            continue;
        }
        const unsigned char *payload = buf + sizeof(*header);
        size_t payload_len = (size_t)n - sizeof(*header);
        if (header->opcode == FUSE_LOOKUP) {
            args->lookup_count++;
        } else if (header->opcode == FUSE_GETATTR) {
            args->getattr_count++;
        } else if (header->opcode == FUSE_FORGET) {
            args->forget_count++;
        } else if (header->opcode == FUSE_READDIR) {
            args->readdir_count++;
        } else if (header->opcode == FUSE_READDIRPLUS) {
            args->readdirplus_count++;
        }
        if (header->opcode == FUSE_READDIR || header->opcode == FUSE_READDIRPLUS) {
            uint32_t trace_index = args->dir_trace_count++;
            if (trace_index < sizeof(args->dir_opcode_trace) / sizeof(args->dir_opcode_trace[0]) &&
                payload_len >= sizeof(struct fuse_read_in)) {
                const struct fuse_read_in *read_in = (const struct fuse_read_in *)payload;
                args->dir_opcode_trace[trace_index] = header->opcode;
                args->dir_offset_trace[trace_index] = read_in->offset;
            }
            (void)p4_handle_readdir(args, header, payload, payload_len);
        } else {
            (void)fuse_handle_one(&args->common, buf, (size_t)n);
        }
    }
    free(buf);
    return NULL;
}

static int fuse_read_launder_counters(const char *path, uint64_t *batches, uint64_t *pages) {
    char *report = fuse_read_stats_report(path);
    if (!report) {
        return -1;
    }
    int result = fuse_parse_u64_counter(report, "invalidation_launder_batches_total", batches);
    if (result == 0) {
        result = fuse_parse_u64_counter(report, "invalidation_launder_pages_total", pages);
    }
    free(report);
    return result;
}

static int fuse_count_mounted_filesystems() {
    FILE *mounts = fopen("/proc/mounts", "r");
    if (!mounts) {
        return -1;
    }
    int count = 0;
    char type[64];
    while (fscanf(mounts, "%*s %*s %63s %*[^\n]\n", type) == 1) {
        if (strcmp(type, "fuse") == 0 || strncmp(type, "fuse.", 5) == 0 ||
            strcmp(type, "fuseblk") == 0 || strcmp(type, "virtiofs") == 0) {
            ++count;
        }
    }
    fclose(mounts);
    return count;
}

static volatile int *fuse_alloc_child_done() {
    void *mapping = mmap(NULL, sizeof(int), PROT_READ | PROT_WRITE,
                         MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (mapping == MAP_FAILED) {
        return NULL;
    }
    memset(mapping, 0, sizeof(int));
    return (volatile int *)mapping;
}

static void fuse_publish_child_done(volatile int *done) {
    __sync_synchronize();
    *done = 1;
    __sync_synchronize();
}

// DragonOS wait4(WNOHANG) is not used as the deadline primitive here: if that
// syscall regresses into a blocking wait, the watchdog itself becomes
// unbounded. The shared flag is published immediately before _exit(), so a
// blocking reap is safe only after the flag is visible or SIGKILL was sent.
static int fuse_reap_child_bounded(pid_t child, volatile int *done, int *status,
                                   unsigned int timeout_ms) {
    if (fuse_wait_flag(done, timeout_ms) == 0) {
        return waitpid(child, status, 0) == child ? 1 : -1;
    }
    if (kill(child, SIGKILL) != 0 && errno != ESRCH) {
        return -1;
    }
    return waitpid(child, status, 0) == child ? 0 : -1;
}

static int fuse_cleanup_mounts_bounded(const char *mp, const char *debug_root) {
    volatile int *done = fuse_alloc_child_done();
    if (!done) {
        return -1;
    }
    pid_t cleaner = fork();
    if (cleaner < 0) {
        munmap((void *)done, sizeof(int));
        return -1;
    }
    if (cleaner == 0) {
        int failed = 0;
        if (umount2(mp, MNT_DETACH) != 0 && errno != EINVAL && errno != ENOENT) {
            failed = 1;
        }
        if (umount2(debug_root, MNT_DETACH) != 0 && errno != EINVAL && errno != ENOENT) {
            failed = 1;
        }
        if (rmdir(mp) != 0 && errno != ENOENT) {
            failed = 1;
        }
        if (rmdir(debug_root) != 0 && errno != ENOENT) {
            failed = 1;
        }
        fuse_publish_child_done(done);
        _exit(failed ? 1 : 0);
    }
    int status = 0;
    int waited = fuse_reap_child_bounded(cleaner, done, &status, 2000);
    munmap((void *)done, sizeof(int));
    if (waited == 1) {
        return WIFEXITED(status) && WEXITSTATUS(status) == 0 ? 0 : -1;
    }
    return -1;
}

static void fuse_sigbus_longjmp_handler(int sig) {
    (void)sig;
    g_fuse_sigbus_seen = 1;
    siglongjmp(g_fuse_sigbus_jmp, 1);
}

static void fuse_sigsegv_longjmp_handler(int sig) {
    (void)sig;
    g_fuse_sigsegv_seen = 1;
    siglongjmp(g_fuse_sigsegv_jmp, 1);
}

#ifndef FUSE_DEV_IOC_CLONE
#define FUSE_DEV_IOC_CLONE 0x8004e500
#endif

#ifndef POSIX_FADV_NOREUSE
#define POSIX_FADV_NOREUSE 5
#endif

#ifndef XATTR_NAME_MAX
#define XATTR_NAME_MAX 255
#endif

#ifndef XATTR_SIZE_MAX
#define XATTR_SIZE_MAX 65536
#endif

#ifndef __NR_sync_file_range
#if defined(__x86_64__)
#define __NR_sync_file_range 277
#elif defined(__riscv) || defined(__loongarch64)
#define __NR_sync_file_range 84
#else
#error "__NR_sync_file_range is not defined for this architecture"
#endif
#endif
#ifndef SYNC_FILE_RANGE_WRITE
#define SYNC_FILE_RANGE_WRITE 2
#endif
#ifndef SYNC_FILE_RANGE_WAIT_AFTER
#define SYNC_FILE_RANGE_WAIT_AFTER 4
#endif

static void fill_user_xattr_name(char *buf, size_t len) {
    memset(buf, 'a', len);
    memcpy(buf, "user.", strlen("user."));
    buf[len] = '\0';
}

static int ext_test_p2_ops() {
    const char *mp = "/tmp/test_fuse_p2_ops";
    int f = -1;
    int dfd = -1;
    int direct_fd = -1;
    ssize_t tn = -1;
    ssize_t rn = -1;
    char hello[256];
    char created[256];
    char symlink_path[256];
    char target_buf[256];
    char hard_path[256];
    char batch_path[256];
    char error_path[256];
    char direct_path[256];
    char sparse_path[256];
    char rbuf[64];
    char dst_exist[256];
    char renamed[256];
    const char extension = 'X';
    const char sparse_marker = 'S';
    const off_t sparse_offset = 5000;
    const size_t sparse_size = (size_t)sparse_offset + 1;
    const size_t batch_size = 3 * 4096 + 17;
    unsigned char *sparse_contents = NULL;
    unsigned char *batch_data = NULL;
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t access_count = 0;
    volatile uint32_t flush_count = 0;
    volatile uint32_t fsync_count = 0;
    volatile uint32_t fsyncdir_count = 0;
    volatile uint32_t create_count = 0;
    volatile uint64_t last_create_nodeid = 0;
    volatile uint32_t rename2_count = 0;
    volatile uint32_t write_count = 0;
    volatile uint32_t dynamic_open_out_flags = 0;
    volatile uint64_t last_write_offset = 0;
    volatile uint32_t last_write_size = 0;
    volatile uint32_t last_write_flags = 0;
    volatile uint32_t write_count_at_fsync = 0;
    volatile uint32_t last_write_flags_at_fsync = 0;
    volatile unsigned char extension_write_byte = 0;
    volatile uint64_t large_write_nodeid = 0;
    volatile uint64_t write_offsets[4] = {0};
    volatile uint32_t write_sizes[4] = {0};
    volatile uint32_t write_flags[4] = {0};
    volatile int forced_write_errno = 0;
    volatile uint64_t forced_write_offset = UINT64_MAX;
    volatile uint64_t forced_short_write_offset = UINT64_MAX;
    volatile uint32_t forced_short_write_size = 0;
    unsigned char batch_backend[16384];
    memset(batch_backend, 0, sizeof(batch_backend));

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;
    args.access_count = &access_count;
    args.flush_count = &flush_count;
    args.fsync_count = &fsync_count;
    args.fsyncdir_count = &fsyncdir_count;
    args.create_count = &create_count;
    args.last_create_nodeid = &last_create_nodeid;
    args.rename2_count = &rename2_count;
    args.write_count = &write_count;
    args.dynamic_open_out_flags = &dynamic_open_out_flags;
    args.last_write_offset = &last_write_offset;
    args.last_write_size = &last_write_size;
    args.last_write_flags = &last_write_flags;
    args.write_count_at_fsync = &write_count_at_fsync;
    args.last_write_flags_at_fsync = &last_write_flags_at_fsync;
    args.large_write_backing = batch_backend;
    args.large_write_backing_capacity = sizeof(batch_backend);
    args.large_write_nodeid = &large_write_nodeid;
    args.write_offsets = write_offsets;
    args.write_sizes = write_sizes;
    args.write_flags = write_flags;
    args.write_trace_capacity = 4;
    args.forced_write_errno = &forced_write_errno;
    args.forced_write_offset = &forced_write_offset;
    args.forced_short_write_offset = &forced_short_write_offset;
    args.forced_short_write_size = &forced_short_write_size;
    args.write_watch_offset = 200;
    args.last_write_watch_byte = &extension_write_byte;
    args.init_out_flags_override = FUSE_INIT_EXT | FUSE_MAX_PAGES | FUSE_WRITEBACK_CACHE;
    args.init_out_max_write_override = 8192;
    args.link_reuse_old_nodeid = 1;
    args.access_deny_mask = 2;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,allow_other", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }

    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        umount(mp);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }

    snprintf(hello, sizeof(hello), "%s/hello.txt", mp);
    if (access(hello, R_OK) != 0) {
        printf("[FAIL] access(R_OK): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (access(hello, W_OK) == 0 || errno != EACCES) {
        printf("[FAIL] access(W_OK) expected EACCES, errno=%d (%s)\n", errno, strerror(errno));
        goto fail;
    }

    snprintf(created, sizeof(created), "%s/p2_create.txt", mp);
    f = open(created, O_CREAT | O_RDWR, 0644);
    if (f < 0) {
        printf("[FAIL] open(O_CREAT): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (fuseg_write_all_fd(f, "p2-data") != 0) {
        printf("[FAIL] write created file: %s (errno=%d)\n", strerror(errno), errno);
        close(f);
        goto fail;
    }
    // LINK returns an attribute snapshot for the same inode.  With
    // writeback-cache negotiated, that daemon-side size is stale until fsync;
    // processing the LINK reply must not roll back the local dirty size.
    snprintf(hard_path, sizeof(hard_path), "%s/p2_hard.txt", mp);
    if (link(created, hard_path) != 0) {
        printf("[FAIL] link dirty writeback file: %s (errno=%d)\n", strerror(errno), errno);
        close(f);
        goto fail;
    }
    if (write_count != 0) {
        printf("[FAIL] writeback-cache write reached daemon before fsync: writes=%u\n",
               write_count);
        close(f);
        goto fail;
    }
    if (fsync(f) != 0) {
        printf("[FAIL] fsync(file): %s (errno=%d)\n", strerror(errno), errno);
        close(f);
        goto fail;
    }
    if (write_count_at_fsync == 0 || last_write_size != strlen("p2-data") ||
        (last_write_flags_at_fsync & FUSE_WRITE_CACHE) == 0) {
        printf("[FAIL] fsync did not drain full cached write first: writes=%u size=%u flags=0x%x\n",
               write_count_at_fsync, last_write_size, last_write_flags_at_fsync);
        close(f);
        goto fail;
    }

    // Exercise writeback of an extension in the original EOF page. The
    // writeback length must be calculated from the extended local size.
    write_count = 0;
    last_write_offset = UINT64_MAX;
    last_write_size = 0;
    write_count_at_fsync = 0;
    if (pwrite(f, &extension, 1, 200) != 1) {
        printf("[FAIL] extend dirty writeback file: %s (errno=%d)\n", strerror(errno), errno);
        close(f);
        goto fail;
    }
    if (fsync(f) != 0) {
        printf("[FAIL] fsync(extended file): %s (errno=%d)\n", strerror(errno), errno);
        close(f);
        goto fail;
    }
    if (write_count_at_fsync == 0 || last_write_size != 201 ||
        extension_write_byte != (unsigned char)extension) {
        printf("[FAIL] fsync truncated extended cached page: writes=%u size=%u byte=%u\n",
               write_count_at_fsync, last_write_size, extension_write_byte);
        close(f);
        goto fail;
    }
    close(f);

    // P3: four locally dirty pages (the last one partial) must be submitted in
    // max_write-sized batches rather than one request per page.
    batch_data = (unsigned char *)malloc(batch_size);
    if (!batch_data) {
        printf("[FAIL] allocate batched write payload\n");
        goto fail;
    }
    for (size_t i = 0; i < batch_size; ++i) {
        batch_data[i] = (unsigned char)(i * 17u + 3u);
    }
    snprintf(batch_path, sizeof(batch_path), "%s/p3_batch.txt", mp);
    f = open(batch_path, O_CREAT | O_RDWR, 0644);
    if (f < 0 || write(f, batch_data, batch_size) != (ssize_t)batch_size) {
        printf("[FAIL] create batched write file: %s (errno=%d)\n", strerror(errno), errno);
        free(batch_data);
        if (f >= 0)
            close(f);
        goto fail;
    }
    large_write_nodeid = last_create_nodeid;
    write_count = 0;
    last_write_offset = UINT64_MAX;
    last_write_size = 0;
    last_write_flags = 0;
    if (fsync(f) != 0) {
        printf("[FAIL] fsync batched write: %s (errno=%d)\n", strerror(errno), errno);
        free(batch_data);
        close(f);
        goto fail;
    }
    if (write_count != 2 || write_offsets[0] != 0 || write_sizes[0] != 8192 ||
        write_offsets[1] != 8192 || write_sizes[1] != batch_size - 8192 ||
        write_flags[0] != FUSE_WRITE_CACHE || write_flags[1] != FUSE_WRITE_CACHE ||
        last_write_offset != 8192 || last_write_size != batch_size - 8192 ||
        (last_write_flags & FUSE_WRITE_CACHE) == 0 || large_write_nodeid == 0 ||
        memcmp(batch_backend, batch_data, batch_size) != 0) {
        printf("[FAIL] batched write requests=%u first=(%llu,%u,0x%x) second=(%llu,%u,0x%x) node=%llu expected_size=%zu\n",
               write_count, (unsigned long long)write_offsets[0], write_sizes[0],
               write_flags[0], (unsigned long long)write_offsets[1], write_sizes[1],
               write_flags[1], (unsigned long long)large_write_nodeid, batch_size);
        free(batch_data);
        close(f);
        goto fail;
    }
    close(f);

    // Exercise the asynchronous WRITE|WAIT_AFTER path. Failure of the second
    // max_write batch must be reported, leave only that failed batch dirty,
    // and permit a later call to retry it without resending the first batch.
    memset(batch_backend, 0, sizeof(batch_backend));
    large_write_nodeid = 0;
    snprintf(error_path, sizeof(error_path), "%s/p3_error.txt", mp);
    f = open(error_path, O_CREAT | O_RDWR, 0644);
    if (f < 0 || write(f, batch_data, batch_size) != (ssize_t)batch_size) {
        printf("[FAIL] create async error file: %s (errno=%d)\n", strerror(errno), errno);
        free(batch_data);
        if (f >= 0)
            close(f);
        goto fail;
    }
    large_write_nodeid = last_create_nodeid;
    write_count = 0;
    forced_write_offset = 8192;
    forced_write_errno = EIO;
    errno = 0;
    if (syscall(__NR_sync_file_range, f, 0, batch_size,
                SYNC_FILE_RANGE_WRITE | SYNC_FILE_RANGE_WAIT_AFTER) != -1 ||
        errno != EIO || write_count != 2 ||
        !((write_offsets[0] == 0 && write_sizes[0] == 8192 &&
           write_offsets[1] == 8192 && write_sizes[1] == batch_size - 8192) ||
          (write_offsets[1] == 0 && write_sizes[1] == 8192 &&
           write_offsets[0] == 8192 && write_sizes[0] == batch_size - 8192))) {
        printf("[FAIL] async batched EIO rc/errno/count=%d/%d/%u first=(%llu,%u) second=(%llu,%u)\n",
               errno == EIO ? -1 : 0, errno, write_count,
               (unsigned long long)write_offsets[0], write_sizes[0],
               (unsigned long long)write_offsets[1], write_sizes[1]);
        forced_write_errno = 0;
        free(batch_data);
        close(f);
        goto fail;
    }
    forced_write_errno = 0;
    forced_write_offset = UINT64_MAX;
    write_count = 0;
    if (syscall(__NR_sync_file_range, f, 0, batch_size,
                SYNC_FILE_RANGE_WRITE | SYNC_FILE_RANGE_WAIT_AFTER) != 0 ||
        write_count != 1 || write_offsets[0] != 8192 ||
        write_sizes[0] != batch_size - 8192 ||
        memcmp(batch_backend, batch_data, batch_size) != 0) {
        printf("[FAIL] async EIO retry count=%u write=(%llu,%u) errno=%d\n", write_count,
               (unsigned long long)write_offsets[0], write_sizes[0], errno);
        free(batch_data);
        close(f);
        goto fail;
    }

    // A short successful reply is an EIO for the whole request. Both pages in
    // that batch must return to Dirty and be resent together on retry.
    if (pwrite(f, batch_data, batch_size, 0) != (ssize_t)batch_size) {
        printf("[FAIL] redirty async short-write file: %s (errno=%d)\n", strerror(errno), errno);
        free(batch_data);
        close(f);
        goto fail;
    }
    write_count = 0;
    forced_short_write_offset = 0;
    forced_short_write_size = 4096;
    errno = 0;
    if (syscall(__NR_sync_file_range, f, 0, batch_size,
                SYNC_FILE_RANGE_WRITE | SYNC_FILE_RANGE_WAIT_AFTER) != -1 ||
        errno != EIO || write_count != 2) {
        printf("[FAIL] async short reply errno/count=%d/%u\n", errno, write_count);
        forced_short_write_offset = UINT64_MAX;
        free(batch_data);
        close(f);
        goto fail;
    }
    forced_short_write_offset = UINT64_MAX;
    forced_short_write_size = 0;
    write_count = 0;
    if (syscall(__NR_sync_file_range, f, 0, batch_size,
                SYNC_FILE_RANGE_WRITE | SYNC_FILE_RANGE_WAIT_AFTER) != 0 ||
        write_count != 1 || write_offsets[0] != 0 || write_sizes[0] != 8192 ||
        memcmp(batch_backend, batch_data, batch_size) != 0) {
        printf("[FAIL] async short retry count=%u write=(%llu,%u) errno=%d\n", write_count,
               (unsigned long long)write_offsets[0], write_sizes[0], errno);
        free(batch_data);
        close(f);
        goto fail;
    }
    close(f);
    f = -1;

    // A direct write must drain overlapping cached dirty pages through the
    // same stable-size batch path before issuing direct FUSE_WRITE requests.
    // With max_write=8192, both phases split as 8192 + 4113 and direct writes
    // must not carry FUSE_WRITE_CACHE. Direct writes do carry the current
    // file lock owner, matching the ordinary FUSE direct-I/O path.
    memset(batch_backend, 0, sizeof(batch_backend));
    large_write_nodeid = 0;
    snprintf(direct_path, sizeof(direct_path), "%s/p3_direct_drain.txt", mp);
    f = open(direct_path, O_CREAT | O_RDWR, 0644);
    if (f < 0 || write(f, batch_data, batch_size) != (ssize_t)batch_size) {
        printf("[FAIL] create dirty direct-drain file: %s (errno=%d)\n", strerror(errno),
               errno);
        free(batch_data);
        goto fail;
    }
    large_write_nodeid = last_create_nodeid;
    write_count = 0;
    dynamic_open_out_flags = FOPEN_DIRECT_IO;
    direct_fd = open(direct_path, O_WRONLY);
    if (direct_fd < 0 || pwrite(direct_fd, batch_data, batch_size, 0) != (ssize_t)batch_size) {
        printf("[FAIL] direct write after dirty batch: %s (errno=%d)\n", strerror(errno),
               errno);
        dynamic_open_out_flags = 0;
        free(batch_data);
        goto fail;
    }
    dynamic_open_out_flags = 0;
    close(direct_fd);
    direct_fd = -1;
    if (write_count != 4 || write_offsets[0] != 0 || write_sizes[0] != 8192 ||
        write_flags[0] != FUSE_WRITE_CACHE || write_offsets[1] != 8192 ||
        write_sizes[1] != batch_size - 8192 || write_flags[1] != FUSE_WRITE_CACHE ||
        write_offsets[2] != 0 || write_sizes[2] != 8192 ||
        write_flags[2] != FUSE_WRITE_LOCKOWNER ||
        write_offsets[3] != 8192 || write_sizes[3] != batch_size - 8192 ||
        write_flags[3] != FUSE_WRITE_LOCKOWNER ||
        memcmp(batch_backend, batch_data, batch_size) != 0) {
        printf("[FAIL] direct drain trace count=%u cached=(%llu,%u,0x%x),(%llu,%u,0x%x) direct=(%llu,%u,0x%x),(%llu,%u,0x%x)\n",
               write_count, (unsigned long long)write_offsets[0], write_sizes[0],
               write_flags[0], (unsigned long long)write_offsets[1], write_sizes[1],
               write_flags[1], (unsigned long long)write_offsets[2], write_sizes[2],
               write_flags[2], (unsigned long long)write_offsets[3], write_sizes[3],
               write_flags[3]);
        free(batch_data);
        goto fail;
    }
    close(f);
    f = -1;
    free(batch_data);
    batch_data = NULL;

    // A short daemon READ inside a locally extended sparse file denotes a
    // hole under FUSE_WRITEBACK_CACHE. It must neither shrink local i_size nor
    // discard the dirty page beyond the hole before writeback reaches daemon.
    snprintf(sparse_path, sizeof(sparse_path), "%s/p2_sparse.txt", mp);
    f = open(sparse_path, O_CREAT | O_RDWR, 0644);
    if (f < 0) {
        printf("[FAIL] open sparse file: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (pwrite(f, &sparse_marker, 1, sparse_offset) != 1) {
        printf("[FAIL] sparse cached extension: %s (errno=%d)\n", strerror(errno), errno);
        close(f);
        goto fail;
    }
    sparse_contents = (unsigned char *)malloc(sparse_size);
    if (!sparse_contents) {
        printf("[FAIL] allocate sparse read buffer\n");
        close(f);
        goto fail;
    }
    memset(sparse_contents, 0xff, sparse_size);
    if (pread(f, sparse_contents, sparse_size, 0) != (ssize_t)sparse_size) {
        printf("[FAIL] read sparse extension before fsync: %s (errno=%d)\n", strerror(errno),
               errno);
        free(sparse_contents);
        close(f);
        goto fail;
    }
    for (size_t i = 0; i < sparse_size - 1; ++i) {
        if (sparse_contents[i] != 0) {
            printf("[FAIL] sparse hole byte %zu is %u\n", i, sparse_contents[i]);
            free(sparse_contents);
            close(f);
            goto fail;
        }
    }
    if (sparse_contents[sparse_size - 1] != (unsigned char)sparse_marker) {
        printf("[FAIL] sparse dirty tail lost before fsync: got=%u\n",
               sparse_contents[sparse_size - 1]);
        free(sparse_contents);
        close(f);
        goto fail;
    }
    free(sparse_contents);
    if (fsync(f) != 0) {
        printf("[FAIL] fsync sparse file: %s (errno=%d)\n", strerror(errno), errno);
        close(f);
        goto fail;
    }
    close(f);

    if (unlink(hard_path) != 0) {
        printf("[FAIL] unlink dirty-link probe: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    snprintf(symlink_path, sizeof(symlink_path), "%s/p2_symlink.txt", mp);
    if (symlink("p2_create.txt", symlink_path) != 0) {
        printf("[FAIL] symlink: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    tn = readlink(symlink_path, target_buf, sizeof(target_buf) - 1);
    if (tn <= 0) {
        printf("[FAIL] readlink: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    target_buf[tn] = '\0';
    if (strcmp(target_buf, "p2_create.txt") != 0) {
        printf("[FAIL] readlink target mismatch: got=%s\n", target_buf);
        goto fail;
    }

    if (link(created, hard_path) != 0) {
        printf("[FAIL] link after fsync: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    if (unlink(created) != 0) {
        printf("[FAIL] unlink original: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    f = open(hard_path, O_RDONLY);
    if (f < 0) {
        printf("[FAIL] open hard link: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    rn = read(f, rbuf, sizeof(rbuf) - 1);
    close(f);
    if (rn < (ssize_t)strlen("p2-data")) {
        printf("[FAIL] read hard link: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (memcmp(rbuf, "p2-data", strlen("p2-data")) != 0) {
        printf("[FAIL] hard link content prefix mismatch\n");
        goto fail;
    }

    snprintf(dst_exist, sizeof(dst_exist), "%s/p2_dst_exist.txt", mp);
    f = open(dst_exist, O_CREAT | O_RDWR, 0644);
    if (f < 0) {
        printf("[FAIL] create dst_exist: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    close(f);

    if (syscall(SYS_renameat2, AT_FDCWD, hard_path, AT_FDCWD, dst_exist, RENAME_NOREPLACE) == 0 ||
        errno != EEXIST) {
        printf("[FAIL] renameat2 NOREPLACE expected EEXIST, errno=%d (%s)\n", errno,
               strerror(errno));
        goto fail;
    }

    snprintf(renamed, sizeof(renamed), "%s/p2_renamed.txt", mp);
    if (syscall(SYS_renameat2, AT_FDCWD, hard_path, AT_FDCWD, renamed, RENAME_NOREPLACE) != 0) {
        printf("[FAIL] renameat2 NOREPLACE success path: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    dfd = open(mp, O_RDONLY | O_DIRECTORY);
    if (dfd < 0) {
        printf("[FAIL] open mountpoint dirfd: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (fsync(dfd) != 0) {
        printf("[FAIL] fsync(dirfd): %s (errno=%d)\n", strerror(errno), errno);
        close(dfd);
        goto fail;
    }
    close(dfd);

    usleep(100 * 1000);

    if (access_count < 2 || flush_count == 0 || fsync_count == 0 || fsyncdir_count == 0 ||
        create_count == 0 || rename2_count < 2) {
        printf("[FAIL] counters access=%u flush=%u fsync=%u fsyncdir=%u create=%u rename2=%u\n",
               access_count, flush_count, fsync_count, fsyncdir_count, create_count,
               rename2_count);
        goto fail;
    }

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail_no_umount;
    }
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (direct_fd >= 0) {
        close(direct_fd);
    }
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_dirty_multibatch_notify_invalidation_inner() {
    char mp[128];
    char debug_root[128];
    const size_t payload_size = 3 * 4096 + 17;
    char path[256];
    char stats_path[256];
    unsigned char payload[3 * 4096 + 17];
    unsigned char backing[16384];
    char first = 0;
    char tail = 0;
    int fd = -1;
    int file = -1;
    int mounted = 0;
    int debug_mounted = 0;
    int daemon_active = 0;
    int fsync_active = 0;
    uint32_t reads_before = 0;
    uint64_t launder_batches_before = 0;
    uint64_t launder_batches_after = 0;
    uint64_t launder_pages_before = 0;
    uint64_t launder_pages_after = 0;
    volatile int stop = 0;
    volatile int init_done = 0;
    volatile int write_entered = 0;
    volatile int release_write = 1;
    volatile uint64_t block_write_offset = 0;
    volatile uint32_t create_count = 0;
    volatile uint64_t last_create_nodeid = 0;
    volatile uint64_t large_write_nodeid = 0;
    volatile uint32_t read_count = 0;
    volatile uint32_t write_count = 0;
    volatile uint64_t write_offsets[4] = {0};
    volatile uint32_t write_sizes[4] = {0};
    volatile uint32_t write_flags[4] = {0};
    pthread_t daemon_thread;
    pthread_t fsync_thread;
    struct fuse_fsync_worker_args fsync_args;
    struct {
        struct fuse_out_header out;
        struct fuse_notify_inval_inode_out inval;
    } notify;

    memset(backing, 0, sizeof(backing));
    for (size_t i = 0; i < payload_size; ++i) {
        payload[i] = (unsigned char)(i * 29u + 7u);
    }
    memset(&fsync_args, 0, sizeof(fsync_args));
    memset(&notify, 0, sizeof(notify));
    snprintf(mp, sizeof(mp), "/tmp/test_fuse_p3_notify_dirty_%d", getpid());
    snprintf(debug_root, sizeof(debug_root), "/tmp/test_fuse_p3_notify_debugfs_%d", getpid());

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }
    if (ensure_dir(debug_root) != 0 || mount("none", debug_root, "debugfs", 0, NULL) != 0) {
        printf("[FAIL] mount debugfs: %s (errno=%d)\n", strerror(errno), errno);
        rmdir(debug_root);
        rmdir(mp);
        return -1;
    }
    debug_mounted = 1;
    snprintf(stats_path, sizeof(stats_path), "%s/fuse/stats", debug_root);
    if (fuse_count_mounted_filesystems() != 0) {
        printf("[FAIL] dirty notify test requires an isolated guest with no FUSE mount\n");
        goto fail;
    }
    fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        umount(debug_root);
        rmdir(debug_root);
        rmdir(mp);
        return -1;
    }

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;
    args.create_count = &create_count;
    args.last_create_nodeid = &last_create_nodeid;
    args.read_count = &read_count;
    args.write_count = &write_count;
    args.write_offsets = write_offsets;
    args.write_sizes = write_sizes;
    args.write_flags = write_flags;
    args.write_trace_capacity = 4;
    args.large_write_backing = backing;
    args.large_write_backing_capacity = sizeof(backing);
    args.large_write_nodeid = &large_write_nodeid;
    args.block_write_offset = &block_write_offset;
    args.write_entered = &write_entered;
    args.release_write = &release_write;
    args.init_out_flags_override = FUSE_INIT_EXT | FUSE_MAX_PAGES | FUSE_WRITEBACK_CACHE;
    args.init_out_max_write_override = 8192;

    if (pthread_create(&daemon_thread, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create daemon\n");
        close(fd);
        umount(debug_root);
        rmdir(debug_root);
        rmdir(mp);
        return -1;
    }
    daemon_active = 1;

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,allow_other",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    mounted = 1;
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }
    if (fuse_count_mounted_filesystems() != 1) {
        printf("[FAIL] dirty notify test lost exclusive FUSE mount ownership\n");
        goto fail;
    }
    if (fuse_read_launder_counters(stats_path, &launder_batches_before,
                                   &launder_pages_before) != 0) {
        printf("[FAIL] read invalidation laundering counters before notify\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/dirty-notify.bin", mp);
    file = open(path, O_CREAT | O_RDWR, 0644);
    if (file < 0 || write(file, payload, payload_size) != (ssize_t)payload_size) {
        printf("[FAIL] create dirty notify file: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    large_write_nodeid = last_create_nodeid;
    if (create_count != 1 || large_write_nodeid == 0 || write_count != 0) {
        printf("[FAIL] dirty setup create=%u node=%llu writes=%u\n", create_count,
               (unsigned long long)large_write_nodeid, write_count);
        goto fail;
    }

    release_write = 0;
    __sync_synchronize();
    notify.out.len = sizeof(notify);
    notify.out.error = FUSE_NOTIFY_INVAL_INODE;
    notify.out.unique = 0;
    notify.inval.ino = large_write_nodeid;
    notify.inval.off = 0;
    notify.inval.len = -1;
    if (write(fd, &notify, sizeof(notify)) != (ssize_t)sizeof(notify)) {
        printf("[FAIL] write dirty inode notify: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (fuse_wait_flag(&write_entered, 5000) != 0) {
        printf("[FAIL] notify worker did not enter laundering WRITE\n");
        goto fail;
    }

    fsync_args.fd = file;
    if (pthread_create(&fsync_thread, NULL, fuse_fsync_worker, &fsync_args) != 0) {
        printf("[FAIL] pthread_create fsync\n");
        goto fail;
    }
    fsync_active = 1;
    release_write = 1;
    __sync_synchronize();
    pthread_join(fsync_thread, NULL);
    fsync_active = 0;
    if (fsync_args.result != 0) {
        errno = fsync_args.saved_errno;
        printf("[FAIL] fsync after dirty notify: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (fuse_count_mounted_filesystems() != 1 ||
        fuse_read_launder_counters(stats_path, &launder_batches_after,
                                   &launder_pages_after) != 0 ||
        launder_batches_after - launder_batches_before != 2 ||
        launder_pages_after - launder_pages_before != 4) {
        printf("[FAIL] notify laundering counter delta batches=%llu->%llu pages=%llu->%llu\n",
               (unsigned long long)launder_batches_before,
               (unsigned long long)launder_batches_after,
               (unsigned long long)launder_pages_before,
               (unsigned long long)launder_pages_after);
        goto fail;
    }

    if (write_count != 2 || write_offsets[0] != 0 || write_sizes[0] != 8192 ||
        write_flags[0] != FUSE_WRITE_CACHE || write_offsets[1] != 8192 ||
        write_sizes[1] != payload_size - 8192 || write_flags[1] != FUSE_WRITE_CACHE ||
        memcmp(backing, payload, payload_size) != 0) {
        printf("[FAIL] notify laundering trace count=%u first=(%llu,%u,0x%x) second=(%llu,%u,0x%x)\n",
               write_count, (unsigned long long)write_offsets[0], write_sizes[0],
               write_flags[0], (unsigned long long)write_offsets[1], write_sizes[1],
               write_flags[1]);
        goto fail;
    }

    // The worker has released the exclusive writeback barrier before fsync
    // can complete. Mutating daemon backing now makes a subsequent cache hit
    // distinguishable from a fresh FUSE_READ after successful invalidation.
    backing[0] = 'Q';
    backing[9000] = 'R';
    reads_before = read_count;
    if (pread(file, &first, 1, 0) != 1 || pread(file, &tail, 1, 9000) != 1 ||
        first != 'Q' || tail != 'R' || read_count <= reads_before) {
        printf("[FAIL] notify did not invalidate cache first=%d tail=%d reads=%u->%u errno=%d\n",
               first, tail, reads_before, read_count, errno);
        goto fail;
    }

    close(file);
    file = -1;
    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail;
    }
    mounted = 0;
    stop = 1;
    close(fd);
    fd = -1;
    pthread_join(daemon_thread, NULL);
    daemon_active = 0;
    umount(debug_root);
    debug_mounted = 0;
    rmdir(debug_root);
    rmdir(mp);
    return 0;

fail:
    release_write = 1;
    __sync_synchronize();
    if (fsync_active) {
        pthread_join(fsync_thread, NULL);
    }
    if (file >= 0) {
        close(file);
    }
    if (mounted) {
        umount(mp);
    }
    stop = 1;
    if (fd >= 0) {
        close(fd);
    }
    if (daemon_active) {
        pthread_join(daemon_thread, NULL);
    }
    if (debug_mounted) {
        umount(debug_root);
    }
    rmdir(debug_root);
    rmdir(mp);
    return -1;
}

static int ext_test_dirty_multibatch_notify_invalidation() {
    char mp[128];
    char debug_root[128];
    volatile int *done = fuse_alloc_child_done();
    if (!done) {
        printf("[FAIL] allocate dirty notify child status: %s (errno=%d)\n", strerror(errno),
               errno);
        return -1;
    }
    pid_t child = fork();
    if (child < 0) {
        printf("[FAIL] fork dirty notify test: %s (errno=%d)\n", strerror(errno), errno);
        munmap((void *)done, sizeof(int));
        return -1;
    }
    if (child == 0) {
        int result = ext_test_dirty_multibatch_notify_invalidation_inner();
        fflush(NULL);
        fuse_publish_child_done(done);
        _exit(result == 0 ? 0 : 1);
    }
    snprintf(mp, sizeof(mp), "/tmp/test_fuse_p3_notify_dirty_%d", child);
    snprintf(debug_root, sizeof(debug_root), "/tmp/test_fuse_p3_notify_debugfs_%d", child);

    int status = 0;
    int waited = fuse_reap_child_bounded(child, done, &status, 15000);
    munmap((void *)done, sizeof(int));
    int result = -1;
    if (waited == 1) {
        result = WIFEXITED(status) && WEXITSTATUS(status) == 0 ? 0 : -1;
    } else {
        printf(waited == 0 ? "[FAIL] dirty notify test timed out\n"
                           : "[FAIL] waitpid dirty notify child failed\n");
    }

    // The child shares the mount namespace. All potentially blocking cleanup
    // runs in its own bounded child so even teardown regressions cannot hang
    // the FuseExtended parent.
    if (fuse_cleanup_mounts_bounded(mp, debug_root) != 0) {
        printf("[FAIL] bounded cleanup of dirty notify mounts failed\n");
        result = -1;
    }
    return result;
}

static int ext_test_positive_lookup_cache_respects_entry_ttl() {
    const char *mp = "/tmp/test_fuse_lookup_cache";
    char hello[256];
    char missing[256];
    struct stat st;
    char buf[32];

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t lookup_count = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.lookup_count = &lookup_count;
    args.entry_valid_sec = 60;
    args.attr_valid_sec = 60;
    args.stop_on_destroy = 1;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }

    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(hello, sizeof(hello), "%s/hello.txt", mp);
    for (int i = 0; i < 3; ++i) {
        if (stat(hello, &st) != 0) {
            printf("[FAIL] stat hello iteration %d: %s (errno=%d)\n", i, strerror(errno), errno);
            goto fail;
        }
        int f = open(hello, O_RDONLY);
        if (f < 0) {
            printf("[FAIL] open hello iteration %d: %s (errno=%d)\n", i, strerror(errno), errno);
            goto fail;
        }
        ssize_t n = read(f, buf, sizeof(buf));
        int saved_errno = errno;
        close(f);
        if (n <= 0) {
            errno = saved_errno;
            printf("[FAIL] read hello iteration %d: %s (errno=%d)\n", i, strerror(errno), errno);
            goto fail;
        }
    }

    if (lookup_count != 1) {
        printf("[FAIL] positive lookup cache expected 1 lookup, got %u\n", lookup_count);
        goto fail;
    }

    snprintf(missing, sizeof(missing), "%s/missing.txt", mp);
    for (int i = 0; i < 2; ++i) {
        if (stat(missing, &st) == 0 || errno != ENOENT) {
            printf("[FAIL] stat missing iteration %d expected ENOENT, errno=%d (%s)\n", i,
                   errno, strerror(errno));
            goto fail;
        }
    }

    if (lookup_count != 3) {
        printf("[FAIL] ordinary ENOENT should not be long-term cached, lookup_count=%u\n",
               lookup_count);
        goto fail;
    }

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail_no_umount;
    }
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_xattr_ops() {
    const char *mp = "/tmp/test_fuse_xattr";
    char path[256];
    char list[64] = {};
    char small[4] = {};
    char value[64] = {};
    char name_255[XATTR_NAME_MAX + 1] = {};
    char name_256[XATTR_NAME_MAX + 2] = {};
    static char value_too_large[XATTR_SIZE_MAX + 1];
    static char max_xattr_buf[XATTR_SIZE_MAX + 1];
    ssize_t n = 0;
    uint32_t set_count_before = 0;

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t getxattr_count = 0;
    volatile uint32_t setxattr_count = 0;
    volatile uint32_t listxattr_count = 0;
    volatile uint32_t removexattr_count = 0;
    volatile uint32_t last_setxattr_flags = UINT32_MAX;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.getxattr_count = &getxattr_count;
    args.setxattr_count = &setxattr_count;
    args.listxattr_count = &listxattr_count;
    args.removexattr_count = &removexattr_count;
    args.last_setxattr_flags = &last_setxattr_flags;
    args.stop_on_destroy = 1;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    errno = 0;
    n = listxattr(path, NULL, 0);
    if (n <= 0) {
        printf("[FAIL] listxattr size returned %zd errno=%d (%s)\n", n, errno, strerror(errno));
        goto fail;
    }
    n = listxattr(path, list, sizeof(list));
    if (n <= 0 || memcmp(list, "user.dragonos", sizeof("user.dragonos")) != 0) {
        printf("[FAIL] listxattr value n=%zd first='%s' errno=%d\n", n, list, errno);
        goto fail;
    }
    if (listxattr_count != 2) {
        printf("[FAIL] listxattr_count=%u expected=2\n", listxattr_count);
        goto fail;
    }

    args.force_listxattr_erange_at_max = 1;
    errno = 0;
    if (listxattr(path, max_xattr_buf, sizeof(max_xattr_buf)) != -1 || errno != E2BIG) {
        printf("[FAIL] listxattr max-size ERANGE errno=%d expected=%d\n", errno, E2BIG);
        goto fail;
    }
    if (listxattr_count != 3) {
        printf("[FAIL] listxattr max-size count=%u expected=3\n", listxattr_count);
        goto fail;
    }
    args.force_listxattr_erange_at_max = 0;

    n = getxattr(path, "user.dragonos", NULL, 0);
    if (n != (ssize_t)strlen("virtiofs-xattr")) {
        printf("[FAIL] getxattr size n=%zd errno=%d (%s)\n", n, errno, strerror(errno));
        goto fail;
    }
    errno = 0;
    if (getxattr(path, "user.dragonos", small, sizeof(small)) != -1 || errno != ERANGE) {
        printf("[FAIL] getxattr small buffer errno=%d expected=%d\n", errno, ERANGE);
        goto fail;
    }
    n = getxattr(path, "user.dragonos", value, sizeof(value));
    if (n != (ssize_t)strlen("virtiofs-xattr") ||
        memcmp(value, "virtiofs-xattr", strlen("virtiofs-xattr")) != 0) {
        printf("[FAIL] getxattr value n=%zd value='%s' errno=%d\n", n, value, errno);
        goto fail;
    }
    if (getxattr_count != 3) {
        printf("[FAIL] getxattr_count=%u expected=3\n", getxattr_count);
        goto fail;
    }

    args.force_getxattr_erange_at_max = 1;
    errno = 0;
    if (getxattr(path, "user.dragonos", max_xattr_buf, sizeof(max_xattr_buf)) != -1 ||
        errno != E2BIG) {
        printf("[FAIL] getxattr max-size ERANGE errno=%d expected=%d\n", errno, E2BIG);
        goto fail;
    }
    if (getxattr_count != 4) {
        printf("[FAIL] getxattr max-size count=%u expected=4\n", getxattr_count);
        goto fail;
    }
    args.force_getxattr_erange_at_max = 0;

    set_count_before = setxattr_count;
    errno = 0;
    if (setxattr(path, "user.dragonos", "new", 3, 0x4) != -1 || errno != EINVAL) {
        printf("[FAIL] setxattr invalid flags errno=%d expected=%d\n", errno, EINVAL);
        goto fail;
    }
    if (setxattr_count != set_count_before) {
        printf("[FAIL] invalid flags reached fuse daemon count=%u before=%u\n", setxattr_count,
               set_count_before);
        goto fail;
    }

    errno = 0;
    if (setxattr(path, "user.dragonos", value_too_large, sizeof(value_too_large), 0) != -1 ||
        errno != E2BIG) {
        printf("[FAIL] setxattr oversized value errno=%d expected=%d\n", errno, E2BIG);
        goto fail;
    }
    if (setxattr_count != set_count_before) {
        printf("[FAIL] oversized value reached fuse daemon count=%u before=%u\n", setxattr_count,
               set_count_before);
        goto fail;
    }

    errno = 0;
    if (setxattr(path, "", "new", 3, 0) != -1 || errno != ERANGE) {
        printf("[FAIL] setxattr empty name errno=%d expected=%d\n", errno, ERANGE);
        goto fail;
    }
    if (setxattr_count != set_count_before) {
        printf("[FAIL] empty name reached fuse daemon count=%u before=%u\n", setxattr_count,
               set_count_before);
        goto fail;
    }

    fill_user_xattr_name(name_255, XATTR_NAME_MAX);
    if (setxattr(path, name_255, "new", 3, 0) != 0) {
        printf("[FAIL] setxattr 255-byte name failed errno=%d (%s)\n", errno, strerror(errno));
        goto fail;
    }
    if (last_setxattr_flags != 0) {
        printf("[FAIL] setxattr 255-byte name flags=%u expected=0\n", last_setxattr_flags);
        goto fail;
    }
    set_count_before = setxattr_count;

    fill_user_xattr_name(name_256, XATTR_NAME_MAX + 1);
    errno = 0;
    if (setxattr(path, name_256, "new", 3, 0) != -1 || errno != ERANGE) {
        printf("[FAIL] setxattr 256-byte name errno=%d expected=%d\n", errno, ERANGE);
        goto fail;
    }
    if (setxattr_count != set_count_before) {
        printf("[FAIL] 256-byte name reached fuse daemon count=%u before=%u\n", setxattr_count,
               set_count_before);
        goto fail;
    }

    if (setxattr(path, "user.zero", nullptr, 0, 0) != 0) {
        printf("[FAIL] setxattr zero-size null value failed errno=%d (%s)\n", errno,
               strerror(errno));
        goto fail;
    }
    if (last_setxattr_flags != 0) {
        printf("[FAIL] setxattr zero-size null flags=%u expected=0\n", last_setxattr_flags);
        goto fail;
    }

    if (setxattr(path, "user.dragonos", "new", 3, 0) != 0) {
        printf("[FAIL] setxattr failed errno=%d (%s)\n", errno, strerror(errno));
        goto fail;
    }
    if (last_setxattr_flags != 0) {
        printf("[FAIL] setxattr flags=%u expected=0\n", last_setxattr_flags);
        goto fail;
    }
    errno = 0;
    if (setxattr(path, "user.dragonos", "new", 3, XATTR_CREATE) != -1 || errno != EEXIST) {
        printf("[FAIL] setxattr XATTR_CREATE errno=%d expected=%d\n", errno, EEXIST);
        goto fail;
    }
    if (last_setxattr_flags != XATTR_CREATE) {
        printf("[FAIL] setxattr flags=%u expected XATTR_CREATE=%d\n", last_setxattr_flags,
               XATTR_CREATE);
        goto fail;
    }
    if (setxattr(path, "user.created", "new", 3, XATTR_CREATE) != 0) {
        printf("[FAIL] setxattr XATTR_CREATE missing failed errno=%d (%s)\n", errno,
               strerror(errno));
        goto fail;
    }
    if (last_setxattr_flags != XATTR_CREATE) {
        printf("[FAIL] setxattr flags=%u expected missing XATTR_CREATE=%d\n",
               last_setxattr_flags, XATTR_CREATE);
        goto fail;
    }
    if (setxattr(path, "user.dragonos", "new", 3, XATTR_REPLACE) != 0) {
        printf("[FAIL] setxattr XATTR_REPLACE failed errno=%d (%s)\n", errno, strerror(errno));
        goto fail;
    }
    if (last_setxattr_flags != XATTR_REPLACE) {
        printf("[FAIL] setxattr flags=%u expected XATTR_REPLACE=%d\n", last_setxattr_flags,
               XATTR_REPLACE);
        goto fail;
    }
    errno = 0;
    if (setxattr(path, "user.missing", "new", 3, XATTR_REPLACE) != -1 || errno != ENODATA) {
        printf("[FAIL] setxattr XATTR_REPLACE missing errno=%d expected=%d\n", errno, ENODATA);
        goto fail;
    }
    if (last_setxattr_flags != XATTR_REPLACE) {
        printf("[FAIL] setxattr flags=%u expected missing XATTR_REPLACE=%d\n",
               last_setxattr_flags, XATTR_REPLACE);
        goto fail;
    }
    if (removexattr(path, "user.dragonos") != 0) {
        printf("[FAIL] removexattr failed errno=%d (%s)\n", errno, strerror(errno));
        goto fail;
    }
    if (setxattr_count != 7 || removexattr_count != 1) {
        printf("[FAIL] set/remove counts set=%u remove=%u\n", setxattr_count, removexattr_count);
        goto fail;
    }

    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_xattr_enosys_is_cached() {
    const char *mp = "/tmp/test_fuse_xattr_enosys";
    char path[256];

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t listxattr_count = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.listxattr_count = &listxattr_count;
    args.force_xattr_enosys = 1;
    args.stop_on_destroy = 1;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    for (int i = 0; i < 2; ++i) {
        errno = 0;
        if (listxattr(path, NULL, 0) != -1 ||
            (errno != EOPNOTSUPP && errno != ENOTSUP)) {
            printf("[FAIL] listxattr ENOSYS cache iter=%d errno=%d (%s)\n", i, errno,
                   strerror(errno));
            goto fail;
        }
    }
    if (listxattr_count != 1) {
        printf("[FAIL] listxattr ENOSYS should be cached, count=%u\n", listxattr_count);
        goto fail;
    }

    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static void ext_sigusr1_handler(int signo) {
    (void)signo;
}

struct ext_reader_ctx {
    char path[256];
    volatile int done;
    ssize_t nread;
    int err;
};

static void *ext_reader_thread(void *arg) {
    struct ext_reader_ctx *ctx = (struct ext_reader_ctx *)arg;
    int fd = open(ctx->path, O_RDONLY);
    if (fd < 0) {
        ctx->nread = -1;
        ctx->err = errno;
        ctx->done = 1;
        return NULL;
    }

    char buf[64];
    ssize_t n = read(fd, buf, sizeof(buf));
    if (n < 0) {
        ctx->nread = -1;
        ctx->err = errno;
    } else {
        ctx->nread = n;
        ctx->err = 0;
    }
    close(fd);
    ctx->done = 1;
    return NULL;
}

static int ext_test_p3_interrupt() {
    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_handler = ext_sigusr1_handler;
    sigemptyset(&sa.sa_mask);
    sa.sa_flags = 0;

    struct sigaction old_sa;
    if (sigaction(SIGUSR1, &sa, &old_sa) != 0) {
        printf("[FAIL] sigaction(SIGUSR1): %s (errno=%d)\n", strerror(errno), errno);
        return -1;
    }

    const char *mp = "/tmp/test_fuse_p3_interrupt";
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        sigaction(SIGUSR1, &old_sa, NULL);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        sigaction(SIGUSR1, &old_sa, NULL);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t interrupt_count = 0;
    volatile uint64_t blocked_read_unique = 0;
    volatile uint64_t last_interrupt_header_unique = 0;
    volatile uint64_t last_interrupt_target = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 0;
    args.stop_on_destroy = 1;
    args.block_read_until_interrupt = 1000;
    args.interrupt_count = &interrupt_count;
    args.blocked_read_unique = &blocked_read_unique;
    args.last_interrupt_header_unique = &last_interrupt_header_unique;
    args.last_interrupt_target = &last_interrupt_target;

    pthread_t daemon_th;
    if (pthread_create(&daemon_th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create(daemon)\n");
        close(fd);
        rmdir(mp);
        sigaction(SIGUSR1, &old_sa, NULL);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(daemon_th, NULL);
        rmdir(mp);
        sigaction(SIGUSR1, &old_sa, NULL);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    struct ext_reader_ctx rctx;
    memset(&rctx, 0, sizeof(rctx));
    snprintf(rctx.path, sizeof(rctx.path), "%s/hello.txt", mp);

    pthread_t reader_th;
    if (pthread_create(&reader_th, NULL, ext_reader_thread, &rctx) != 0) {
        printf("[FAIL] pthread_create(reader)\n");
        goto fail;
    }

    for (int i = 0; i < 200; i++) {
        if (blocked_read_unique != 0) {
            break;
        }
        usleep(5 * 1000);
    }
    if (blocked_read_unique == 0) {
        printf("[FAIL] timed out waiting for blocked read request\n");
        stop = 1;
        pthread_join(reader_th, NULL);
        goto fail;
    }

    if (pthread_kill(reader_th, SIGUSR1) != 0) {
        printf("[FAIL] pthread_kill(SIGUSR1)\n");
        stop = 1;
        pthread_join(reader_th, NULL);
        goto fail;
    }
    pthread_join(reader_th, NULL);

    if (rctx.nread != -1 || rctx.err != EINTR) {
        printf("[FAIL] reader expected EINTR, nread=%zd err=%d (%s)\n", rctx.nread, rctx.err,
               strerror(rctx.err));
        goto fail;
    }

    for (int i = 0; i < 500; i++) {
        if (interrupt_count > 0) {
            break;
        }
        usleep(5 * 1000);
    }

    if (interrupt_count == 0) {
        printf("[FAIL] expected FUSE_INTERRUPT request\n");
        goto fail;
    }
    if (last_interrupt_target == 0 || last_interrupt_target != blocked_read_unique) {
        printf("[FAIL] interrupt target mismatch: blocked=%llu interrupt_target=%llu\n",
               (unsigned long long)blocked_read_unique, (unsigned long long)last_interrupt_target);
        goto fail;
    }
    if (last_interrupt_header_unique != (blocked_read_unique | 1ULL)) {
        printf("[FAIL] interrupt header unique mismatch: blocked=%llu header=%llu\n",
               (unsigned long long)blocked_read_unique,
               (unsigned long long)last_interrupt_header_unique);
        goto fail;
    }

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail_no_umount;
    }
    stop = 1;
    close(fd);
    pthread_join(daemon_th, NULL);
    rmdir(mp);
    sigaction(SIGUSR1, &old_sa, NULL);
    return 0;

fail:
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(daemon_th, NULL);
    rmdir(mp);
    sigaction(SIGUSR1, &old_sa, NULL);
    return -1;
}

static int ext_test_p3_noopen_readdirplus_notify() {
    const char *mp = "/tmp/test_fuse_p3_noopen";
    ssize_t wn = -1;
    ssize_t verify_n = -1;
    int f = -1;
    uint32_t reads_before_inval = 0;
    uint32_t lookups_before_inval = 0;
    size_t entry_notify_len = 0;
    void *private_map = MAP_FAILED;
    char verify_buf[64];
    struct {
        struct fuse_out_header out;
        struct fuse_notify_inval_entry_out inval;
        char name[sizeof("hello.txt")];
    } entry_notify;
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t open_count = 0;
    volatile uint32_t opendir_count = 0;
    volatile uint32_t release_count = 0;
    volatile uint32_t releasedir_count = 0;
    volatile uint32_t readdirplus_count = 0;
    volatile uint32_t lookup_count = 0;
    volatile uint32_t read_count = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 0;
    args.stop_on_destroy = 1;
    args.open_count = &open_count;
    args.opendir_count = &opendir_count;
    args.release_count = &release_count;
    args.releasedir_count = &releasedir_count;
    args.readdirplus_count = &readdirplus_count;
    args.lookup_count = &lookup_count;
    args.read_count = &read_count;
    args.force_open_enosys = 1;
    args.force_opendir_enosys = 1;
    args.entry_valid_sec = 60;
    args.attr_valid_sec = 60;
    args.init_out_flags_override = FUSE_INIT_EXT | FUSE_MAX_PAGES | FUSE_NO_OPEN_SUPPORT |
                                   FUSE_NO_OPENDIR_SUPPORT | FUSE_DO_READDIRPLUS;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    char file_path[256];
    snprintf(file_path, sizeof(file_path), "%s/hello.txt", mp);
    for (int i = 0; i < 2; i++) {
        int f = open(file_path, O_RDONLY);
        if (f < 0) {
            printf("[FAIL] open(%s): %s (errno=%d)\n", file_path, strerror(errno), errno);
            goto fail;
        }
        char buf[64];
        ssize_t n = read(f, buf, sizeof(buf) - 1);
        close(f);
        if (n <= 0) {
            printf("[FAIL] read(%s): %s (errno=%d)\n", file_path, strerror(errno), errno);
            goto fail;
        }
    }

    f = open(file_path, O_RDONLY);
    if (f < 0 || read(f, verify_buf, sizeof(verify_buf)) <= 0) {
        printf("[FAIL] keep-open read before notify: %s (errno=%d)\n", strerror(errno), errno);
        if (f >= 0) close(f);
        goto fail;
    }
    private_map = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE, f, 0);
    if (private_map == MAP_FAILED) {
        printf("[FAIL] MAP_PRIVATE before notify: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    ((volatile char *)private_map)[0] = 'P';

    for (int i = 0; i < 2; i++) {
        DIR *dir = opendir(mp);
        if (!dir) {
            printf("[FAIL] opendir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
            goto fail;
        }
        int saw = 0;
        struct dirent *de;
        while ((de = readdir(dir)) != NULL) {
            if (strcmp(de->d_name, "hello.txt") == 0) {
                saw = 1;
            }
        }
        closedir(dir);
        if (!saw) {
            printf("[FAIL] readdir didn't see hello.txt\n");
            goto fail;
        }
    }

    struct {
        struct fuse_out_header out;
        struct fuse_notify_inval_inode_out inval;
    } notify_msg;
    memset(&notify_msg, 0, sizeof(notify_msg));
    notify_msg.out.len = sizeof(notify_msg);
    notify_msg.out.error = FUSE_NOTIFY_INVAL_INODE;
    notify_msg.out.unique = 0;
    notify_msg.inval.ino = 2;
    notify_msg.inval.off = 0;
    notify_msg.inval.len = -1;
    wn = write(fd, &notify_msg, sizeof(notify_msg));
    if (wn != (ssize_t)sizeof(notify_msg)) {
        printf("[FAIL] write notify: wn=%zd errno=%d (%s)\n", wn, errno, strerror(errno));
        goto fail;
    }

    usleep(100 * 1000);

    reads_before_inval = read_count;
    if (lseek(f, 0, SEEK_SET) < 0) {
        printf("[FAIL] lseek after inode notify: %s (errno=%d)\n", strerror(errno), errno);
        close(f);
        goto fail;
    }
    verify_n = read(f, verify_buf, sizeof(verify_buf));
    close(f);
    f = -1;
    if (verify_n <= 0 || read_count <= reads_before_inval) {
        printf("[FAIL] inode notify did not force a fresh READ: before=%u after=%u n=%zd\n",
               reads_before_inval, read_count, verify_n);
        goto fail;
    }
    if (((volatile char *)private_map)[0] != 'P') {
        printf("[FAIL] inode notify discarded MAP_PRIVATE COW data\n");
        goto fail;
    }

    memset(&entry_notify, 0, sizeof(entry_notify));
    entry_notify_len = offsetof(decltype(entry_notify), name) + sizeof(entry_notify.name);
    entry_notify.out.len = entry_notify_len;
    entry_notify.out.error = FUSE_NOTIFY_INVAL_ENTRY;
    entry_notify.inval.parent = 1;
    entry_notify.inval.namelen = strlen("hello.txt");
    memcpy(entry_notify.name, "hello.txt", sizeof("hello.txt"));
    lookups_before_inval = lookup_count;
    wn = write(fd, &entry_notify, entry_notify_len);
    if (wn != (ssize_t)entry_notify_len) {
        printf("[FAIL] write entry notify: wn=%zd errno=%d (%s)\n", wn, errno,
               strerror(errno));
        goto fail;
    }
    f = open(file_path, O_RDONLY);
    if (f < 0) {
        printf("[FAIL] open after entry notify: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    close(f);
    if (lookup_count <= lookups_before_inval) {
        printf("[FAIL] entry notify did not force a fresh LOOKUP: before=%u after=%u\n",
               lookups_before_inval, lookup_count);
        goto fail;
    }

    if (open_count != 1 || opendir_count != 1 || release_count != 0 || releasedir_count != 0 ||
        readdirplus_count == 0) {
        printf("[FAIL] counters open=%u opendir=%u release=%u releasedir=%u readdirplus=%u\n",
               open_count, opendir_count, release_count, releasedir_count, readdirplus_count);
        goto fail;
    }

    munmap(private_map, 4096);
    private_map = MAP_FAILED;
    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail_no_umount;
    }
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (private_map != MAP_FAILED) {
        munmap(private_map, 4096);
    }
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_open_zero_fh_valid() {
    const char *mp = "/tmp/test_fuse_zero_fh";
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t open_count = 0;
    volatile uint32_t read_count = 0;
    volatile uint64_t last_open_fh = UINT64_MAX;
    volatile uint64_t last_read_fh = UINT64_MAX;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.stop_on_destroy = 1;
    args.open_count = &open_count;
    args.read_count = &read_count;
    args.last_open_fh = &last_open_fh;
    args.last_read_fh = &last_read_fh;
    args.has_hello_open_fh_override = 1;
    args.hello_open_fh_override = 0;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    char path[256];
    char buf[128];
    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    if (fuseg_read_file_cstr(path, buf, sizeof(buf)) < 0) {
        printf("[FAIL] read(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    if (strcmp(buf, "hello from fuse\n") != 0) {
        printf("[FAIL] content mismatch: got='%s'\n", buf);
        goto fail;
    }

    usleep(100 * 1000);
    if (open_count == 0 || read_count == 0 || last_open_fh != 0 || last_read_fh != 0) {
        printf("[FAIL] fh counters open=%u read=%u open_fh=%llu read_fh=%llu\n", open_count,
               read_count, (unsigned long long)last_open_fh, (unsigned long long)last_read_fh);
        goto fail;
    }

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail_no_umount;
    }
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_noopen_fsync_uses_zero_fh() {
    const char *mp = "/tmp/test_fuse_noopen_fsync";
    int f = -1;
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t open_count = 0;
    volatile uint32_t fsync_count = 0;
    volatile uint32_t release_count = 0;
    volatile uint64_t last_fsync_fh = UINT64_MAX;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.stop_on_destroy = 1;
    args.open_count = &open_count;
    args.fsync_count = &fsync_count;
    args.release_count = &release_count;
    args.last_fsync_fh = &last_fsync_fh;
    args.force_open_enosys = 1;
    args.init_out_flags_override = FUSE_INIT_EXT | FUSE_MAX_PAGES | FUSE_NO_OPEN_SUPPORT;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    char path[256];
    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDONLY);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    if (fsync(f) != 0) {
        printf("[FAIL] fsync(no-open file): %s (errno=%d)\n", strerror(errno), errno);
        close(f);
        goto fail;
    }
    close(f);

    usleep(100 * 1000);
    if (open_count != 1 || fsync_count == 0 || release_count != 0 || last_fsync_fh != 0) {
        printf("[FAIL] counters open=%u fsync=%u release=%u fsync_fh=%llu\n", open_count,
               fsync_count, release_count, (unsigned long long)last_fsync_fh);
        goto fail;
    }

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail_no_umount;
    }
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_noopen_close_flushes_dirty_data_with_zero_fh() {
    const char *mp = "/tmp/test_fuse_noopen_close_flush";
    const char marker = 'N';
    int f = -1;
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t open_count = 0;
    volatile uint32_t flush_count = 0;
    volatile uint32_t release_count = 0;
    volatile uint32_t write_count = 0;
    volatile uint32_t write_count_at_flush = 0;
    volatile uint32_t last_write_flags = 0;
    volatile uint64_t last_flush_fh = UINT64_MAX;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;
    args.open_count = &open_count;
    args.flush_count = &flush_count;
    args.release_count = &release_count;
    args.write_count = &write_count;
    args.write_count_at_flush = &write_count_at_flush;
    args.last_write_flags = &last_write_flags;
    args.last_flush_fh = &last_flush_fh;
    args.force_open_enosys = 1;
    args.init_out_flags_override =
        FUSE_INIT_EXT | FUSE_MAX_PAGES | FUSE_NO_OPEN_SUPPORT | FUSE_WRITEBACK_CACHE;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    char path[256];
    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDWR);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    if (pwrite(f, &marker, 1, 1) != 1) {
        printf("[FAIL] pwrite(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    if (write_count != 0) {
        printf("[FAIL] no-open WB write reached daemon before close: writes=%u\n", write_count);
        goto fail;
    }
    if (close(f) != 0) {
        printf("[FAIL] close(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        f = -1;
        goto fail;
    }
    f = -1;

    if (open_count != 1 || write_count_at_flush == 0 || flush_count != 1 ||
        last_flush_fh != 0 || release_count != 0 ||
        (last_write_flags & FUSE_WRITE_CACHE) == 0) {
        printf("[FAIL] no-open close ordering open=%u writes=%u writes_at_flush=%u flush=%u flush_fh=%llu release=%u flags=0x%x\n",
               open_count, write_count, write_count_at_flush, flush_count,
               (unsigned long long)last_flush_fh, release_count, last_write_flags);
        goto fail;
    }

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail_no_umount;
    }
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (f >= 0) {
        close(f);
    }
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_fsync_enosys_cached_success() {
    const char *mp = "/tmp/test_fuse_fsync_enosys";
    int f = -1;
    int dfd = -1;
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t fsync_count = 0;
    volatile uint32_t fsyncdir_count = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.stop_on_destroy = 1;
    args.fsync_count = &fsync_count;
    args.fsyncdir_count = &fsyncdir_count;
    args.force_fsync_errno = ENOSYS;
    args.force_fsyncdir_errno = ENOSYS;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    char path[256];
    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDONLY);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    if (fsync(f) != 0 || fsync(f) != 0) {
        printf("[FAIL] fsync(file ENOSYS cache): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    close(f);
    f = -1;

    dfd = open(mp, O_RDONLY | O_DIRECTORY);
    if (dfd < 0) {
        printf("[FAIL] open dirfd(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail;
    }
    if (fsync(dfd) != 0 || fsync(dfd) != 0) {
        printf("[FAIL] fsync(dir ENOSYS cache): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    close(dfd);
    dfd = -1;

    if (fsync_count != 1 || fsyncdir_count != 1) {
        printf("[FAIL] ENOSYS fsync cache counters fsync=%u fsyncdir=%u\n", fsync_count,
               fsyncdir_count);
        goto fail;
    }

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail_no_umount;
    }
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (f >= 0) {
        close(f);
    }
    if (dfd >= 0) {
        close(dfd);
    }
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_open_release_flags_match_linux() {
    const char *mp = "/tmp/test_fuse_open_flags";
    int requested = O_RDWR | O_NOCTTY | O_TRUNC | O_APPEND | O_NONBLOCK;
    uint32_t expected_open = (uint32_t)(requested & ~(O_CREAT | O_EXCL | O_NOCTTY));
    int f = -1;
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t last_open_flags = 0;
    volatile uint32_t last_release_flags = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;
    args.last_open_in_flags = &last_open_flags;
    args.last_release_in_flags = &last_release_flags;
    args.init_out_flags_override = FUSE_INIT_EXT | FUSE_MAX_PAGES | FUSE_ATOMIC_O_TRUNC;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    char path[256];
    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, requested);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    close(f);

    usleep(100 * 1000);
    if (last_open_flags != expected_open) {
        printf("[FAIL] open flags got=0%o expected=0%o\n", last_open_flags, expected_open);
        goto fail;
    }
    if (last_release_flags != (uint32_t)requested) {
        printf("[FAIL] release flags got=0%o expected=0%o\n", last_release_flags,
               (uint32_t)requested);
        goto fail;
    }

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail_no_umount;
    }
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_create_reuses_fuse_handle() {
    const char *mp = "/tmp/test_fuse_create_handle";
    const uint64_t create_fh = 0xcafe2019ULL;
    int requested = O_CREAT | O_RDWR | O_TRUNC | O_APPEND | O_NONBLOCK | O_NOCTTY | O_CLOEXEC;
    uint32_t expected_create = (uint32_t)(requested & ~(O_NOCTTY | O_CLOEXEC));
    int f = -1;
    if (ensure_dir(mp) != 0) return -1;
    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0, init_done = 0;
    volatile uint32_t create_count = 0, open_count = 0, write_count = 0;
    volatile uint32_t flush_count = 0, release_count = 0, setattr_count = 0, create_flags = 0;
    volatile uint64_t write_fh = 0, flush_fh = 0, release_fh = 0;
    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;
    args.create_count = &create_count;
    args.open_count = &open_count;
    args.write_count = &write_count;
    args.flush_count = &flush_count;
    args.release_count = &release_count;
    args.setattr_count = &setattr_count;
    args.last_create_in_flags = &create_flags;
    args.last_write_fh = &write_fh;
    args.last_flush_fh = &flush_fh;
    args.last_release_fh = &release_fh;
    args.create_open_fh_override = create_fh;
    args.init_out_flags_override = FUSE_INIT_EXT | FUSE_MAX_PAGES | FUSE_ATOMIC_O_TRUNC;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) goto fail_no_thread;
    char opts[256], path[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) goto fail_thread;
    if (fuseg_wait_init(&init_done) != 0) goto fail;
    snprintf(path, sizeof(path), "%s/new.txt", mp);
    f = open(path, requested, 0644);
    if (f < 0 || write(f, "x", 1) != 1) goto fail;
    close(f);
    f = -1;
    for (int i = 0; i < 200 && release_count < 1; i++) {
        usleep(10 * 1000);
    }

    if (create_count != 1 || open_count != 0 || write_count != 1 || flush_count != 1 ||
        release_count != 1 || setattr_count != 0 || create_flags != expected_create ||
        write_fh != create_fh || flush_fh != create_fh || release_fh != create_fh) {
        printf("[FAIL] create handle reuse create=%u open=%u write=%u flush=%u release=%u "
               "flags=0%o expected=0%o fhs=%llx/%llx/%llx\n",
               create_count, open_count, write_count, flush_count, release_count, create_flags,
               expected_create, (unsigned long long)write_fh, (unsigned long long)flush_fh,
               (unsigned long long)release_fh);
        goto fail;
    }
    if (unlink(path) != 0) goto fail;
    if (umount(mp) != 0) goto fail_no_umount;
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (f >= 0) close(f);
    umount(mp);
fail_no_umount:
    stop = 1;
fail_thread:
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
fail_no_thread:
    close(fd);
    rmdir(mp);
    return -1;
}

static int ext_test_create_enosys_falls_back_and_caches() {
    const char *mp = "/tmp/test_fuse_create_enosys";
    if (ensure_dir(mp) != 0) return -1;
    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        rmdir(mp);
        return -1;
    }
    volatile int stop = 0, init_done = 0;
    volatile uint32_t create_count = 0, mknod_count = 0, open_count = 0, release_count = 0;
    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;
    args.force_create_errno = ENOSYS;
    args.create_count = &create_count;
    args.mknod_count = &mknod_count;
    args.open_count = &open_count;
    args.release_count = &release_count;
    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) goto fail_no_thread;
    char opts[256], path[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) goto fail_thread;
    if (fuseg_wait_init(&init_done) != 0) goto fail;
    for (int i = 0; i < 2; i++) {
        snprintf(path, sizeof(path), "%s/fallback-%d", mp, i);
        int f = open(path, O_CREAT | O_RDWR, 0644);
        if (f < 0) goto fail;
        close(f);
    }
    for (int i = 0; i < 200 && release_count < 2; i++) {
        usleep(10 * 1000);
    }
    if (create_count != 1 || mknod_count != 2 || open_count != 2 || release_count != 2) {
        printf("[FAIL] create ENOSYS cache create=%u mknod=%u open=%u release=%u\n",
               create_count, mknod_count, open_count, release_count);
        goto fail;
    }
    if (umount(mp) != 0) goto fail_no_umount;
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;
fail:
    umount(mp);
fail_no_umount:
    stop = 1;
fail_thread:
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
fail_no_thread:
    close(fd);
    rmdir(mp);
    return -1;
}

static int ext_test_invalid_create_reply_cleans_resources() {
    const char *mp = "/tmp/test_fuse_create_cleanup";
    const uint64_t create_fh = 0xbad2019ULL;
    int bad_fd = -1;
    if (ensure_dir(mp) != 0) return -1;
    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        rmdir(mp);
        return -1;
    }
    volatile int stop = 0, init_done = 0;
    volatile uint32_t create_count = 0, open_count = 0, release_count = 0, forget_count = 0;
    volatile uint64_t release_fh = 0, forget_nlookup = 0;
    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;
    args.create_count = &create_count;
    args.open_count = &open_count;
    args.release_count = &release_count;
    args.forget_count = &forget_count;
    args.forget_nlookup_sum = &forget_nlookup;
    args.last_release_fh = &release_fh;
    args.create_open_fh_override = create_fh;
    args.create_reply_mode_override = S_IFDIR | 0755;
    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) goto fail_no_thread;
    char opts[256], path[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) goto fail_thread;
    if (fuseg_wait_init(&init_done) != 0) goto fail;
    snprintf(path, sizeof(path), "%s/invalid", mp);
    errno = 0;
    bad_fd = open(path, O_CREAT | O_RDWR, 0644);
    if (bad_fd >= 0) {
        close(bad_fd);
        goto fail;
    }
    if (errno != EIO) goto fail;
    for (int i = 0; i < 200 && (release_count < 1 || forget_count < 1); i++) {
        usleep(10 * 1000);
    }
    if (create_count != 1 || open_count != 0 || release_count != 1 || release_fh != create_fh ||
        forget_count != 1 || forget_nlookup != 1) {
        printf("[FAIL] invalid CREATE cleanup create=%u open=%u release=%u fh=%llx "
               "forget=%u nlookup=%llu\n",
               create_count, open_count, release_count, (unsigned long long)release_fh,
               forget_count, (unsigned long long)forget_nlookup);
        goto fail;
    }
    if (umount(mp) != 0) goto fail_no_umount;
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;
fail:
    umount(mp);
fail_no_umount:
    stop = 1;
fail_thread:
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
fail_no_thread:
    close(fd);
    rmdir(mp);
    return -1;
}

static int ext_test_fsetfl_updates_fuse_io_flags() {
    const char *mp = "/tmp/test_fuse_fsetfl_flags";
    int requested = O_RDWR;
    int f = -1;
    int old_flags = -1;
    uint32_t expected_open = (uint32_t)requested;
    uint32_t expected_setfl = 0;
    char buf[8];
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t open_count = 0;
    volatile uint32_t read_count = 0;
    volatile uint32_t write_count = 0;
    volatile uint32_t flush_count = 0;
    volatile uint32_t release_count = 0;
    volatile uint32_t last_open_flags = 0;
    volatile uint32_t last_read_flags = 0;
    volatile uint32_t last_write_flags = 0;
    volatile uint32_t last_flush_uid = UINT32_MAX;
    volatile uint32_t last_flush_gid = UINT32_MAX;
    volatile uint32_t last_flush_pid = 0;
    volatile uint32_t last_release_flags = 0;
    volatile uint32_t last_release_uid = UINT32_MAX;
    volatile uint32_t last_release_gid = UINT32_MAX;
    volatile uint32_t last_release_pid = UINT32_MAX;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;
    args.open_count = &open_count;
    args.read_count = &read_count;
    args.write_count = &write_count;
    args.flush_count = &flush_count;
    args.release_count = &release_count;
    args.last_open_in_flags = &last_open_flags;
    args.last_read_open_flags = &last_read_flags;
    args.last_write_open_flags = &last_write_flags;
    args.last_flush_uid = &last_flush_uid;
    args.last_flush_gid = &last_flush_gid;
    args.last_flush_pid = &last_flush_pid;
    args.last_release_in_flags = &last_release_flags;
    args.last_release_uid = &last_release_uid;
    args.last_release_gid = &last_release_gid;
    args.last_release_pid = &last_release_pid;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    char path[256];
    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, requested);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }

    old_flags = fcntl(f, F_GETFL);
    if (old_flags < 0) {
        printf("[FAIL] fcntl(F_GETFL): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (fcntl(f, F_SETFL, old_flags | O_NONBLOCK) != 0) {
        printf("[FAIL] fcntl(F_SETFL): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    memset(buf, 0, sizeof(buf));
    if (read(f, buf, 5) != 5 || memcmp(buf, "hello", 5) != 0) {
        printf("[FAIL] read after F_SETFL got='%.*s' errno=%d\n", 5, buf, errno);
        goto fail;
    }
    if (write(f, "X", 1) != 1) {
        printf("[FAIL] write after F_SETFL: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    close(f);
    f = -1;
    usleep(100 * 1000);

    expected_setfl = (uint32_t)(old_flags | O_NONBLOCK);
    if (open_count != 1 || read_count != 1 || write_count != 1 || flush_count != 1 ||
        release_count != 1) {
        printf("[FAIL] counters open=%u read=%u write=%u flush=%u release=%u\n", open_count,
               read_count, write_count, flush_count, release_count);
        goto fail;
    }
    if (last_open_flags != expected_open) {
        printf("[FAIL] open flags got=0%o expected=0%o\n", last_open_flags, expected_open);
        goto fail;
    }
    if ((last_read_flags & O_NONBLOCK) == 0 || last_write_flags != expected_setfl ||
        last_release_flags != expected_setfl) {
        printf("[FAIL] updated flags read=0%o write=0%o release=0%o expected=0%o\n",
               last_read_flags, last_write_flags, last_release_flags, expected_setfl);
        goto fail;
    }
    if (last_flush_uid != 0 || last_flush_gid != 0 || last_flush_pid == 0) {
        printf("[FAIL] flush should use caller credentials uid=%u gid=%u pid=%u\n",
               last_flush_uid, last_flush_gid, last_flush_pid);
        goto fail;
    }
    if (last_release_uid != 0 || last_release_gid != 0 || last_release_pid != 0) {
        printf("[FAIL] release should use nocreds uid=%u gid=%u pid=%u\n", last_release_uid,
               last_release_gid, last_release_pid);
        goto fail;
    }

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail_no_umount;
    }
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (f >= 0) {
        close(f);
    }
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_fsetfl_updates_fuse_dev_nonblock() {
    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        return -1;
    }

    int old_flags = fcntl(fd, F_GETFL);
    if (old_flags < 0) {
        printf("[FAIL] fcntl(F_GETFL): %s (errno=%d)\n", strerror(errno), errno);
        close(fd);
        return -1;
    }
    if ((old_flags & O_NONBLOCK) != 0) {
        printf("[FAIL] /dev/fuse unexpectedly opened nonblocking: flags=0%o\n", old_flags);
        close(fd);
        return -1;
    }
    if (fcntl(fd, F_SETFL, old_flags | O_NONBLOCK) != 0) {
        printf("[FAIL] fcntl(F_SETFL O_NONBLOCK): %s (errno=%d)\n", strerror(errno), errno);
        close(fd);
        return -1;
    }

    pid_t child = fork();
    if (child < 0) {
        printf("[FAIL] fork: %s (errno=%d)\n", strerror(errno), errno);
        close(fd);
        return -1;
    }
    if (child == 0) {
        unsigned char *buf = (unsigned char *)malloc(FUSE_TEST_BUF_SIZE);
        if (!buf) {
            _exit(11);
        }
        ssize_t n = read(fd, buf, FUSE_TEST_BUF_SIZE);
        int saved_errno = errno;
        free(buf);
        if (n < 0 && (saved_errno == EAGAIN || saved_errno == EWOULDBLOCK)) {
            _exit(0);
        }
        _exit(12);
    }

    for (int i = 0; i < 50; i++) {
        int status = 0;
        pid_t got = waitpid(child, &status, WNOHANG);
        if (got == child) {
            close(fd);
            if (WIFEXITED(status) && WEXITSTATUS(status) == 0) {
                return 0;
            }
            printf("[FAIL] child read did not return EAGAIN, status=%d\n", status);
            return -1;
        }
        if (got < 0) {
            printf("[FAIL] waitpid: %s (errno=%d)\n", strerror(errno), errno);
            close(fd);
            return -1;
        }
        usleep(20 * 1000);
    }

    kill(child, SIGKILL);
    waitpid(child, NULL, 0);
    close(fd);
    printf("[FAIL] /dev/fuse read blocked after F_SETFL O_NONBLOCK\n");
    return -1;
}

static int ext_test_fopen_noflush_skips_flush() {
    const char *mp = "/tmp/test_fuse_noflush";
    int f = -1;
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t flush_count = 0;
    volatile uint32_t release_count = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.stop_on_destroy = 1;
    args.flush_count = &flush_count;
    args.release_count = &release_count;
    args.hello_open_out_flags = FOPEN_NOFLUSH;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    char path[256];
    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDONLY);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    close(f);
    f = -1;

    usleep(100 * 1000);
    if (flush_count != 0 || release_count != 1) {
        printf("[FAIL] noflush counters flush=%u release=%u\n", flush_count, release_count);
        goto fail;
    }

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail_no_umount;
    }
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (f >= 0) {
        close(f);
    }
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_writeback_cache_noflush_still_flushes() {
    const char *mp = "/tmp/test_fuse_wb_noflush_flush";
    char path[256];
    int f = -1;
    int fd = -1;
    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t flush_count = 0;
    volatile uint32_t write_count = 0;
    volatile uint32_t write_count_at_flush = 0;
    volatile uint32_t last_write_flags = 0;
    const char marker = 'W';
    pthread_t th;
    int thread_started = 0;
    int mounted = 0;

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }
    fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;
    args.flush_count = &flush_count;
    args.write_count = &write_count;
    args.write_count_at_flush = &write_count_at_flush;
    args.last_write_flags = &last_write_flags;
    args.hello_open_out_flags = FOPEN_NOFLUSH;
    args.init_out_flags_override = FUSE_INIT_EXT | FUSE_MAX_PAGES | FUSE_WRITEBACK_CACHE;

    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }
    thread_started = 1;

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    mounted = 1;
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDWR);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    if (pwrite(f, &marker, 1, 1) != 1) {
        printf("[FAIL] pwrite(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    if (write_count != 0) {
        printf("[FAIL] writeback-cache write reached daemon before close: writes=%u\n",
               write_count);
        goto fail;
    }
    if (close(f) != 0) {
        printf("[FAIL] close(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        f = -1;
        goto fail;
    }
    f = -1;

    if (flush_count != 1 || write_count_at_flush == 0 ||
        (last_write_flags & FUSE_WRITE_CACHE) == 0) {
        printf("[FAIL] WB+NOFLUSH ordering writes=%u writes_at_flush=%u flush=%u flags=0x%x\n",
               write_count, write_count_at_flush, flush_count, last_write_flags);
        goto fail;
    }

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail;
    }
    mounted = 0;
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (f >= 0) {
        close(f);
    }
    if (mounted) {
        umount(mp);
    }
    stop = 1;
    close(fd);
    if (thread_started) {
        pthread_join(th, NULL);
    }
    rmdir(mp);
    return -1;
}

static int ext_test_nonwriteback_mmap_close_flushes_dirty_mapping() {
    const char *mp = "/tmp/test_fuse_nonwb_mmap_close";
    char path[256];
    int f = -1;
    int fd = -1;
    void *addr = MAP_FAILED;
    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t flush_count = 0;
    volatile uint32_t read_count = 0;
    volatile uint32_t write_count = 0;
    volatile uint32_t write_count_at_flush = 0;
    volatile uint32_t last_write_flags = 0;
    pthread_t th;
    int thread_started = 0;
    int mounted = 0;

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }
    fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;
    args.flush_count = &flush_count;
    args.read_count = &read_count;
    args.write_count = &write_count;
    args.write_count_at_flush = &write_count_at_flush;
    args.last_write_flags = &last_write_flags;
    args.init_out_flags_override = FUSE_INIT_EXT | FUSE_MAX_PAGES;

    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }
    thread_started = 1;

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    mounted = 1;
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDWR);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    addr = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, f, 0);
    if (addr == MAP_FAILED) {
        printf("[FAIL] mmap(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    if (((volatile char *)addr)[0] != 'h') {
        printf("[FAIL] non-WB shared mmap first byte got=%d\n",
               ((volatile char *)addr)[0]);
        goto fail;
    }
    ((volatile char *)addr)[3] = 'C';
    if (write_count != 0) {
        printf("[FAIL] dirty mmap reached daemon before close: writes=%u\n", write_count);
        goto fail;
    }
    if (close(f) != 0) {
        printf("[FAIL] close(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        f = -1;
        goto fail;
    }
    f = -1;

    if (((volatile char *)addr)[3] != 'C' || read_count != 1 || flush_count != 1 ||
        write_count_at_flush == 0 || (last_write_flags & FUSE_WRITE_CACHE) == 0) {
        printf("[FAIL] non-WB mmap close ordering reads=%u writes=%u writes_at_flush=%u flush=%u flags=0x%x mapped=%d\n",
               read_count, write_count, write_count_at_flush, flush_count, last_write_flags,
               ((volatile char *)addr)[3]);
        goto fail;
    }

    if (munmap(addr, 4096) != 0) {
        printf("[FAIL] munmap(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    addr = MAP_FAILED;
    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail;
    }
    mounted = 0;
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (addr != MAP_FAILED) {
        munmap(addr, 4096);
    }
    if (f >= 0) {
        close(f);
    }
    if (mounted) {
        umount(mp);
    }
    stop = 1;
    close(fd);
    if (thread_started) {
        pthread_join(th, NULL);
    }
    rmdir(mp);
    return -1;
}

static int ext_test_close_returns_flush_error_and_closes_fd() {
    const char *mp = "/tmp/test_fuse_close_flush_error";
    int f = -1;
    int oldfd = -1;
    int rc = 0;
    char tmp = 0;
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t flush_count = 0;
    volatile uint32_t release_count = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.stop_on_destroy = 1;
    args.flush_count = &flush_count;
    args.release_count = &release_count;
    args.force_flush_errno = EIO;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    char path[256];
    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDONLY);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }

    errno = 0;
    oldfd = f;
    rc = close(f);
    f = -1;
    if (rc != -1 || errno != EIO) {
        printf("[FAIL] close should return EIO rc=%d errno=%d\n", rc, errno);
        goto fail;
    }
    errno = 0;
    if (read(oldfd, &tmp, 1) != -1 || errno != EBADF) {
        printf("[FAIL] close error must still close fd read_errno=%d\n", errno);
        goto fail;
    }

    usleep(100 * 1000);
    if (flush_count != 1 || release_count != 1) {
        printf("[FAIL] close flush error counters flush=%u release=%u\n", flush_count,
               release_count);
        goto fail;
    }

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail_no_umount;
    }
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (f >= 0) {
        close(f);
    }
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_flush_enosys_cached_success() {
    const char *mp = "/tmp/test_fuse_flush_enosys";
    int f = -1;
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t flush_count = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.stop_on_destroy = 1;
    args.flush_count = &flush_count;
    args.force_flush_errno = ENOSYS;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    char path[256];
    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    for (int i = 0; i < 2; ++i) {
        f = open(path, O_RDONLY);
        if (f < 0) {
            printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
            goto fail;
        }
        if (close(f) != 0) {
            printf("[FAIL] close after FLUSH ENOSYS: %s (errno=%d)\n", strerror(errno), errno);
            f = -1;
            goto fail;
        }
        f = -1;
    }

    usleep(100 * 1000);
    if (flush_count != 1) {
        printf("[FAIL] FLUSH ENOSYS should be cached, flush_count=%u\n", flush_count);
        goto fail;
    }

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail_no_umount;
    }
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (f >= 0) {
        close(f);
    }
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_fopen_nonseekable_mode(uint32_t open_out_flags, const char *mp,
                                           int expect_stream) {
    int f = -1;
    char buf[8];
    ssize_t n = -1;
    volatile uint64_t last_write_offset = UINT64_MAX;
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;
    args.hello_open_out_flags = open_out_flags;
    args.last_write_offset = &last_write_offset;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    char path[256];
    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDWR);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }

    errno = 0;
    if (lseek(f, 0, SEEK_SET) >= 0 || errno != ESPIPE) {
        printf("[FAIL] lseek expected ESPIPE, ret errno=%d (%s)\n", errno, strerror(errno));
        goto fail;
    }
    errno = 0;
    if (pread(f, buf, 1, 0) >= 0 || errno != ESPIPE) {
        printf("[FAIL] pread expected ESPIPE, errno=%d (%s)\n", errno, strerror(errno));
        goto fail;
    }
    errno = 0;
    if (pwrite(f, "x", 1, 0) >= 0 || errno != ESPIPE) {
        printf("[FAIL] pwrite expected ESPIPE, errno=%d (%s)\n", errno, strerror(errno));
        goto fail;
    }

    memset(buf, 0, sizeof(buf));
    if (read(f, buf, 5) != 5 || memcmp(buf, "hello", 5) != 0) {
        printf("[FAIL] ordinary read failed got='%.*s' errno=%d\n", 5, buf, errno);
        goto fail;
    }
    memset(buf, 0, sizeof(buf));
    n = read(f, buf, 5);
    if (expect_stream) {
        if (n != 5 || memcmp(buf, "hello", 5) != 0) {
            printf("[FAIL] stream read did not restart at offset 0 got n=%zd data='%.*s' errno=%d\n",
                   n, 5, buf, errno);
            goto fail;
        }
        if (write(f, "Z", 1) != 1) {
            printf("[FAIL] stream write failed: %s (errno=%d)\n", strerror(errno), errno);
            goto fail;
        }
        if (last_write_offset != 0) {
            printf("[FAIL] stream write offset expected 0 got %llu\n",
                   (unsigned long long)last_write_offset);
            goto fail;
        }
    } else if (n != 5 || memcmp(buf, " from", 5) != 0) {
        printf("[FAIL] nonseekable sequential read should advance offset got n=%zd data='%.*s'\n", n,
               5, buf);
        goto fail;
    }

    close(f);
    f = -1;
    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail_no_umount;
    }
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (f >= 0) {
        close(f);
    }
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_fopen_nonseekable_dir_mode(uint32_t open_out_flags, const char *mp) {
    int f = -1;
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t releasedir_count = 0;
    volatile uint32_t last_releasedir_uid = UINT32_MAX;
    volatile uint32_t last_releasedir_gid = UINT32_MAX;
    volatile uint32_t last_releasedir_pid = UINT32_MAX;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.stop_on_destroy = 1;
    args.root_open_out_flags = open_out_flags;
    args.releasedir_count = &releasedir_count;
    args.last_releasedir_uid = &last_releasedir_uid;
    args.last_releasedir_gid = &last_releasedir_gid;
    args.last_releasedir_pid = &last_releasedir_pid;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    f = open(mp, O_RDONLY | O_DIRECTORY);
    if (f < 0) {
        printf("[FAIL] open(%s, O_DIRECTORY): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail;
    }

    errno = 0;
    if (lseek(f, 0, SEEK_SET) >= 0 || errno != ESPIPE) {
        printf("[FAIL] dir lseek expected ESPIPE, errno=%d (%s)\n", errno, strerror(errno));
        goto fail;
    }

    close(f);
    f = -1;
    usleep(100 * 1000);

    if (releasedir_count != 1 || last_releasedir_uid != 0 || last_releasedir_gid != 0 ||
        last_releasedir_pid != 0) {
        printf("[FAIL] releasedir nocreds count=%u uid=%u gid=%u pid=%u\n", releasedir_count,
               last_releasedir_uid, last_releasedir_gid, last_releasedir_pid);
        goto fail;
    }

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail_no_umount;
    }
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (f >= 0) {
        close(f);
    }
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_atomic_otrunc_uses_open_without_setattr() {
    const char *mp = "/tmp/test_fuse_atomic_otrunc";
    int requested = O_RDWR | O_TRUNC;
    int f = -1;
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t last_open_flags = 0;
    volatile uint32_t open_count = 0;
    volatile uint32_t setattr_count = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 0;
    args.stop_on_destroy = 1;
    args.open_count = &open_count;
    args.setattr_count = &setattr_count;
    args.last_open_in_flags = &last_open_flags;
    args.init_out_flags_override = FUSE_INIT_EXT | FUSE_MAX_PAGES | FUSE_ATOMIC_O_TRUNC;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    char path[256];
    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, requested);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    close(f);
    f = -1;

    usleep(100 * 1000);
    if (open_count != 1 || (last_open_flags & O_TRUNC) == 0) {
        printf("[FAIL] open counters/flags open=%u flags=0%o\n", open_count, last_open_flags);
        goto fail;
    }
    if (setattr_count != 0) {
        printf("[FAIL] atomic O_TRUNC unexpectedly sent SETATTR count=%u\n", setattr_count);
        goto fail;
    }

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail_no_umount;
    }
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (f >= 0) {
        close(f);
    }
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_ftruncate_setattr_uses_open_fh() {
    const char *mp = "/tmp/test_fuse_ftruncate_fh";
    int f = -1;
    char fallocate_verify[17] = {};
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t open_count = 0;
    volatile uint32_t setattr_count = 0;
    volatile uint32_t fallocate_count = 0;
    volatile uint32_t write_count = 0;
    volatile uint64_t last_open_fh = 0;
    volatile uint32_t last_setattr_valid = 0;
    volatile uint64_t last_setattr_fh = 0;
    volatile uint64_t last_setattr_size = 0;
    volatile uint64_t last_setattr_lock_owner = 0;
    volatile uint64_t last_fallocate_fh = 0;
    volatile uint64_t last_fallocate_offset = 0;
    volatile uint64_t last_fallocate_length = 0;
    volatile uint32_t last_fallocate_mode = 0;
    volatile uint64_t last_write_offset = 0;
    volatile uint32_t last_write_size = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;
    args.open_count = &open_count;
    args.setattr_count = &setattr_count;
    args.fallocate_count = &fallocate_count;
    args.write_count = &write_count;
    args.last_open_fh = &last_open_fh;
    args.last_setattr_valid = &last_setattr_valid;
    args.last_setattr_fh = &last_setattr_fh;
    args.last_setattr_size = &last_setattr_size;
    args.last_setattr_lock_owner = &last_setattr_lock_owner;
    args.last_fallocate_fh = &last_fallocate_fh;
    args.last_fallocate_offset = &last_fallocate_offset;
    args.last_fallocate_length = &last_fallocate_length;
    args.last_fallocate_mode = &last_fallocate_mode;
    args.last_write_offset = &last_write_offset;
    args.last_write_size = &last_write_size;
    args.next_open_fh = 940;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    char path[256];
    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDWR);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    if (ftruncate(f, 7) != 0) {
        printf("[FAIL] ftruncate: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    close(f);
    f = -1;

    usleep(100 * 1000);
    if (open_count != 1 || setattr_count != 1) {
        printf("[FAIL] counters open=%u setattr=%u\n", open_count, setattr_count);
        goto fail;
    }
    if ((last_setattr_valid & FATTR_SIZE) == 0 || (last_setattr_valid & FATTR_FH) == 0 ||
        (last_setattr_valid & FATTR_LOCKOWNER) == 0 || last_setattr_fh != 940 ||
        last_setattr_size != 7 || last_setattr_lock_owner == 0) {
        printf("[FAIL] setattr valid=0x%x fh=%llu size=%llu lock_owner=%llu\n",
               last_setattr_valid, (unsigned long long)last_setattr_fh,
               (unsigned long long)last_setattr_size,
               (unsigned long long)last_setattr_lock_owner);
        goto fail;
    }

    last_setattr_valid = 0;
    last_setattr_fh = 0;
    last_setattr_size = 0;
    last_setattr_lock_owner = 0;
    if (truncate(path, 5) != 0) {
        printf("[FAIL] truncate(path): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    usleep(100 * 1000);
    if (setattr_count != 2) {
        printf("[FAIL] path truncate setattr_count=%u\n", setattr_count);
        goto fail;
    }
    if ((last_setattr_valid & FATTR_SIZE) == 0 || (last_setattr_valid & FATTR_FH) != 0 ||
        (last_setattr_valid & FATTR_LOCKOWNER) == 0 || last_setattr_size != 5 ||
        last_setattr_lock_owner == 0) {
        printf("[FAIL] path setattr valid=0x%x fh=%llu size=%llu lock_owner=%llu\n",
               last_setattr_valid, (unsigned long long)last_setattr_fh,
               (unsigned long long)last_setattr_size,
               (unsigned long long)last_setattr_lock_owner);
        goto fail;
    }

    last_setattr_valid = 0;
    last_setattr_fh = 0;
    last_setattr_size = 0;
    last_setattr_lock_owner = 0;
    f = open(path, O_RDWR | O_TRUNC);
    if (f < 0) {
        printf("[FAIL] open(O_TRUNC): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    close(f);
    f = -1;
    usleep(100 * 1000);
    if (setattr_count != 3) {
        printf("[FAIL] open(O_TRUNC) setattr_count=%u\n", setattr_count);
        goto fail;
    }
    if ((last_setattr_valid & FATTR_SIZE) == 0 || (last_setattr_valid & FATTR_FH) != 0 ||
        (last_setattr_valid & FATTR_LOCKOWNER) == 0 || last_setattr_size != 0 ||
        last_setattr_lock_owner == 0) {
        printf("[FAIL] open truncate setattr valid=0x%x fh=%llu size=%llu lock_owner=%llu\n",
               last_setattr_valid, (unsigned long long)last_setattr_fh,
               (unsigned long long)last_setattr_size,
               (unsigned long long)last_setattr_lock_owner);
        goto fail;
    }

    setattr_count = 0;
    fallocate_count = 0;
    last_open_fh = 0;
    last_fallocate_fh = 0;
    last_fallocate_offset = 0;
    last_fallocate_length = 0;
    last_fallocate_mode = 0;
    f = open(path, O_RDWR);
    if (f < 0) {
        printf("[FAIL] open for fallocate: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (syscall(SYS_fallocate, f, 0, 0, 16) != 0) {
        printf("[FAIL] fallocate: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    close(f);
    f = -1;
    usleep(100 * 1000);
    if (setattr_count != 0 || fallocate_count != 1 || last_fallocate_fh != last_open_fh ||
        last_fallocate_offset != 0 || last_fallocate_length != 16 || last_fallocate_mode != 0) {
        printf("[FAIL] fallocate counters setattr=%u fallocate=%u fh=%llu open_fh=%llu "
               "offset=%llu length=%llu mode=%u\n",
               setattr_count, fallocate_count, (unsigned long long)last_fallocate_fh,
               (unsigned long long)last_open_fh, (unsigned long long)last_fallocate_offset,
               (unsigned long long)last_fallocate_length, last_fallocate_mode);
        goto fail;
    }
    struct stat st;
    if (stat(path, &st) != 0 || st.st_size != 16) {
        printf("[FAIL] stat after fallocate rc/size errno=%d (%s) size=%lld\n", errno,
               strerror(errno), (long long)st.st_size);
        goto fail;
    }

    static const char fallocate_pattern[] = "abcdefghijklmnop";
    f = open(path, O_RDWR);
    if (f < 0) {
        printf("[FAIL] reopen for fallocate modes: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (pwrite(f, fallocate_pattern, sizeof(fallocate_pattern) - 1, 0) !=
        (ssize_t)(sizeof(fallocate_pattern) - 1)) {
        printf("[FAIL] seed fallocate cache test: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (pread(f, fallocate_verify, sizeof(fallocate_pattern) - 1, 0) !=
        (ssize_t)(sizeof(fallocate_pattern) - 1)) {
        printf("[FAIL] prime fallocate cache: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    fallocate_count = 0;
    if (syscall(SYS_fallocate, f, FALLOC_FL_KEEP_SIZE, 0, 64) != 0) {
        printf("[FAIL] fallocate KEEP_SIZE: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (stat(path, &st) != 0 || st.st_size != 16 || fallocate_count != 1 ||
        last_fallocate_mode != FALLOC_FL_KEEP_SIZE) {
        printf("[FAIL] KEEP_SIZE semantics size=%lld count=%u mode=0x%x\n",
               (long long)st.st_size, fallocate_count, last_fallocate_mode);
        goto fail;
    }

    fallocate_count = 0;
    if (syscall(SYS_fallocate, f, FALLOC_FL_ZERO_RANGE | FALLOC_FL_KEEP_SIZE, 4, 4) != 0) {
        printf("[FAIL] fallocate ZERO_RANGE: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    memset(fallocate_verify, 0xff, sizeof(fallocate_verify));
    if (pread(f, fallocate_verify, sizeof(fallocate_pattern) - 1, 0) !=
        (ssize_t)(sizeof(fallocate_pattern) - 1) ||
        memcmp(fallocate_verify, "abcd\0\0\0\0ijklmnop", sizeof(fallocate_pattern) - 1) != 0 ||
        fallocate_count != 1 ||
        last_fallocate_mode != (FALLOC_FL_ZERO_RANGE | FALLOC_FL_KEEP_SIZE)) {
        printf("[FAIL] ZERO_RANGE cache/forwarding count=%u mode=0x%x\n", fallocate_count,
               last_fallocate_mode);
        goto fail;
    }

    fallocate_count = 0;
    if (syscall(SYS_fallocate, f, FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, 8, 4) != 0) {
        printf("[FAIL] fallocate PUNCH_HOLE: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    memset(fallocate_verify, 0xff, sizeof(fallocate_verify));
    if (pread(f, fallocate_verify, sizeof(fallocate_pattern) - 1, 0) !=
        (ssize_t)(sizeof(fallocate_pattern) - 1) ||
        memcmp(fallocate_verify, "abcd\0\0\0\0\0\0\0\0mnop", sizeof(fallocate_pattern) - 1) !=
            0 ||
        fallocate_count != 1 ||
        last_fallocate_mode != (FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE)) {
        printf("[FAIL] PUNCH_HOLE cache/forwarding count=%u mode=0x%x\n", fallocate_count,
               last_fallocate_mode);
        goto fail;
    }
    close(f);
    f = -1;

    setattr_count = 0;
    fallocate_count = 0;
    f = open(path, O_RDWR);
    if (f < 0) {
        printf("[FAIL] open for fallocate overflow: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (syscall(SYS_fallocate, f, 0, INT64_MAX - 1, 4) == 0 || errno != EFBIG) {
        printf("[FAIL] fallocate overflow expected EFBIG, errno=%d (%s)\n", errno,
               strerror(errno));
        goto fail;
    }
    close(f);
    f = -1;
    usleep(100 * 1000);
    if (setattr_count != 0 || fallocate_count != 0) {
        printf("[FAIL] fallocate overflow sent requests setattr=%u fallocate=%u\n", setattr_count,
               fallocate_count);
        goto fail;
    }

    setattr_count = 0;
    last_setattr_valid = 0;
    last_setattr_fh = 0;
    last_setattr_size = 0;
    last_setattr_lock_owner = 0;
    write_count = 0;
    last_write_offset = 0;
    last_write_size = 0;
    f = open(path, O_RDWR);
    if (f < 0) {
        printf("[FAIL] open for pwrite: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (pwrite(f, "xy", 2, 9) != 2) {
        printf("[FAIL] pwrite hole: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    close(f);
    f = -1;
    usleep(100 * 1000);
    if (setattr_count != 0 || write_count != 1 || last_write_offset != 9 || last_write_size != 2) {
        printf("[FAIL] pwrite hole counters setattr=%u write=%u offset=%llu size=%u\n",
               setattr_count, write_count, (unsigned long long)last_write_offset,
               last_write_size);
        goto fail;
    }

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail_no_umount;
    }
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (f >= 0) {
        close(f);
    }
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_init_requests_linux_no_open_support() {
    const char *mp = "/tmp/test_fuse_init_flags";
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t init_flags = 0;
    volatile uint32_t init_flags2 = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.stop_on_destroy = 1;
    args.init_in_flags = &init_flags;
    args.init_in_flags2 = &init_flags2;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    if ((init_flags & FUSE_NO_OPEN_SUPPORT) == 0 ||
        (init_flags & FUSE_NO_OPENDIR_SUPPORT) == 0 ||
        (init_flags & FUSE_WRITEBACK_CACHE) == 0 ||
        (init_flags2 & (1u << (35 - 32))) == 0) {
        printf("[FAIL] INIT flags missing no-open/writeback/expire-only support bits: "
               "flags=0x%x flags2=0x%x\n",
               init_flags, init_flags2);
        goto fail;
    }

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail_no_umount;
    }
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_p4_subtype_mount() {
    const char *mp = "/tmp/test_fuse_p4_subtype";
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 0;
    args.stop_on_destroy = 1;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse.fuse3_demo", 0, opts) != 0) {
        printf("[FAIL] mount(fuse.fuse3_demo): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }

    for (int i = 0; i < 200; i++) {
        if (init_done) {
            break;
        }
        usleep(10 * 1000);
    }
    if (!init_done) {
        printf("[FAIL] init handshake timeout\n");
        umount(mp);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }

    char file_path[256];
    snprintf(file_path, sizeof(file_path), "%s/hello.txt", mp);

    char buf[128];
    if (fuseg_read_file_cstr(file_path, buf, sizeof(buf)) < 0) {
        printf("[FAIL] read(%s): %s (errno=%d)\n", file_path, strerror(errno), errno);
        umount(mp);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (strcmp(buf, "hello from fuse\n") != 0) {
        printf("[FAIL] content mismatch: got='%s'\n", buf);
        umount(mp);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }

    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;
}

static int ext_run_child_drop_priv_and_stat(const char *mp, int expect_errno, int expect_success) {
    pid_t pid = fork();
    if (pid < 0) {
        return -1;
    }
    if (pid == 0) {
        if (setgid(1000) != 0) {
            _exit(30);
        }
        if (setuid(1000) != 0) {
            _exit(31);
        }

        struct stat st;
        int r = stat(mp, &st);
        if (expect_success) {
            if (r != 0)
                _exit(10);
            char p[256];
            snprintf(p, sizeof(p), "%s/hello.txt", mp);
            int fd = open(p, O_RDONLY);
            if (fd < 0)
                _exit(11);
            char buf[64];
            ssize_t n = read(fd, buf, sizeof(buf) - 1);
            close(fd);
            if (n < 0)
                _exit(12);
            buf[n] = '\0';
            if (strcmp(buf, "hello from fuse\n") != 0)
                _exit(13);
            _exit(0);
        }

        if (r != 0 && errno == expect_errno) {
            _exit(0);
        }
        if (r != 0) {
            _exit(21);
        }

        /*
         * Linux 语义下，目录本身的 stat 可能成功；真正的拒绝点通常体现在
         * 访问目录内对象（例如 open/stat 子路径）。
         */
        char p[256];
        snprintf(p, sizeof(p), "%s/hello.txt", mp);
        int fd = open(p, O_RDONLY);
        if (fd >= 0) {
            close(fd);
            _exit(22);
        }
        if (errno != expect_errno) {
            _exit(23);
        }
        _exit(0);
    }

    int status = 0;
    if (waitpid(pid, &status, 0) < 0) {
        return -1;
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        errno = ECHILD;
        return -1;
    }
    return 0;
}

static int ext_run_permission_case(const char *mp, const char *opts, uint32_t root_mode_override,
                                   uint32_t hello_mode_override, int expect_errno,
                                   int expect_success) {
    if (ensure_dir(mp) != 0) {
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 0;
    args.exit_after_init = 0;
    args.stop_on_destroy = 1;
    args.root_mode_override = root_mode_override;
    args.hello_mode_override = hello_mode_override;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        close(fd);
        rmdir(mp);
        return -1;
    }

    char full_opts[512];
    snprintf(full_opts, sizeof(full_opts), "fd=%d,%s", fd, opts);
    if (mount("none", mp, "fuse", 0, full_opts) != 0) {
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }

    if (fuseg_wait_init(&init_done) != 0) {
        umount(mp);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }

    if (ext_run_child_drop_priv_and_stat(mp, expect_errno, expect_success) != 0) {
        umount(mp);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }

    umount(mp);
    rmdir(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    return 0;
}

static int ext_test_permissions() {
    const uint32_t DIR_NO_PERM = 0040000;
    const uint32_t REG_NO_PERM = 0100000;

    {
        const char *mp = "/tmp/test_fuse_perm_owner";
        if (ext_run_permission_case(mp, "rootmode=040755,user_id=0,group_id=0", 0, 0, EACCES, 0) !=
            0) {
            printf("[FAIL] mount owner restriction\n");
            return -1;
        }
    }

    {
        const char *mp = "/tmp/test_fuse_perm_default";
        if (ext_run_permission_case(
                mp, "rootmode=040000,user_id=0,group_id=0,allow_other,default_permissions",
                DIR_NO_PERM, REG_NO_PERM, EACCES, 0) != 0) {
            printf("[FAIL] default_permissions deny\n");
            return -1;
        }
    }

    {
        const char *mp = "/tmp/test_fuse_perm_remote";
        if (ext_run_permission_case(mp, "rootmode=040000,user_id=0,group_id=0,allow_other",
                                    DIR_NO_PERM, REG_NO_PERM, 0, 1) != 0) {
            printf("[FAIL] remote permission model allow\n");
            return -1;
        }
    }

    return 0;
}

static int ext_test_clone() {
    const char *mp = "/tmp/test_fuse_clone";
    DIR *d = NULL;
    int found = 0;
    struct dirent *de = NULL;
    char p[256];
    struct stat st;
    char buf[128];
    int n = -1;
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int master_fd = open("/dev/fuse", O_RDWR);
    if (master_fd < 0) {
        printf("[FAIL] open(/dev/fuse master): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;

    struct fuse_daemon_args master_args;
    memset(&master_args, 0, sizeof(master_args));
    master_args.fd = master_fd;
    master_args.stop = &stop;
    master_args.init_done = &init_done;
    master_args.enable_write_ops = 0;
    master_args.exit_after_init = 1;

    pthread_t master_th;
    if (pthread_create(&master_th, NULL, fuse_daemon_thread, &master_args) != 0) {
        printf("[FAIL] pthread_create(master)\n");
        close(master_fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", master_fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(master_fd);
        pthread_join(master_th, NULL);
        rmdir(mp);
        return -1;
    }

    for (int i = 0; i < 100; i++) {
        if (init_done)
            break;
        usleep(10 * 1000);
    }
    if (!init_done) {
        printf("[FAIL] init handshake timeout\n");
        umount(mp);
        stop = 1;
        close(master_fd);
        pthread_join(master_th, NULL);
        rmdir(mp);
        return -1;
    }

    pthread_join(master_th, NULL);

    int clone_fd = open("/dev/fuse", O_RDWR);
    if (clone_fd < 0) {
        printf("[FAIL] open(/dev/fuse clone): %s (errno=%d)\n", strerror(errno), errno);
        umount(mp);
        close(master_fd);
        rmdir(mp);
        return -1;
    }

    uint32_t oldfd_u32 = (uint32_t)master_fd;
    if (ioctl(clone_fd, FUSE_DEV_IOC_CLONE, &oldfd_u32) != 0) {
        printf("[FAIL] ioctl(FUSE_DEV_IOC_CLONE): %s (errno=%d)\n", strerror(errno), errno);
        umount(mp);
        close(clone_fd);
        close(master_fd);
        rmdir(mp);
        return -1;
    }

    struct fuse_daemon_args clone_args;
    memset(&clone_args, 0, sizeof(clone_args));
    clone_args.fd = clone_fd;
    clone_args.stop = &stop;
    clone_args.init_done = &init_done;
    clone_args.enable_write_ops = 0;
    clone_args.exit_after_init = 0;
    clone_args.stop_on_destroy = 1;

    pthread_t clone_th;
    if (pthread_create(&clone_th, NULL, fuse_daemon_thread, &clone_args) != 0) {
        printf("[FAIL] pthread_create(clone)\n");
        umount(mp);
        close(clone_fd);
        close(master_fd);
        rmdir(mp);
        return -1;
    }

    d = opendir(mp);
    if (!d) {
        printf("[FAIL] opendir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail;
    }
    found = 0;
    while ((de = readdir(d)) != NULL) {
        if (strcmp(de->d_name, "hello.txt") == 0) {
            found = 1;
            break;
        }
    }
    closedir(d);
    if (!found) {
        printf("[FAIL] readdir: hello.txt not found\n");
        goto fail;
    }

    snprintf(p, sizeof(p), "%s/hello.txt", mp);
    if (stat(p, &st) != 0) {
        printf("[FAIL] stat(%s): %s (errno=%d)\n", p, strerror(errno), errno);
        goto fail;
    }
    if (!S_ISREG(st.st_mode)) {
        printf("[FAIL] stat: expected regular file\n");
        goto fail;
    }

    n = fuseg_read_file_cstr(p, buf, sizeof(buf));
    if (n < 0) {
        printf("[FAIL] read(%s): %s (errno=%d)\n", p, strerror(errno), errno);
        goto fail;
    }
    if (strcmp(buf, "hello from fuse\n") != 0) {
        printf("[FAIL] content mismatch: got='%s'\n", buf);
        goto fail;
    }

    umount(mp);
    rmdir(mp);
    stop = 1;
    close(clone_fd);
    close(master_fd);
    pthread_join(clone_th, NULL);
    return 0;

fail:
    umount(mp);
    stop = 1;
    close(clone_fd);
    close(master_fd);
    pthread_join(clone_th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_large_read_over_max_write() {
    const char *mp = "/tmp/test_fuse_large_read";
    const size_t data_size = 6000;
    char path[256];
    char *buf = NULL;
    int n = -1;

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t read_count = 0;
    volatile uint64_t read_offsets[4] = {0};
    volatile uint32_t read_sizes[4] = {0};

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.read_count = &read_count;
    args.read_offsets = read_offsets;
    args.read_sizes = read_sizes;
    args.read_trace_capacity = 4;
    args.hello_data_size_override = data_size;
    args.stop_on_destroy = 1;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    buf = (char *)malloc(data_size);
    if (!buf) {
        printf("[FAIL] malloc read buffer\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    n = fuseg_read_file(path, buf, data_size);
    if (n < 0) {
        printf("[FAIL] read(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    if ((size_t)n != data_size) {
        printf("[FAIL] read size mismatch: got=%d expected=%zu read_count=%u\n", n, data_size,
               read_count);
        goto fail;
    }
    for (size_t i = 0; i < data_size; i++) {
        char expected = (char)('A' + (i % 26));
        if (buf[i] != expected) {
            printf("[FAIL] read data mismatch at %zu: got=%d expected=%d\n", i, buf[i],
                   expected);
            goto fail;
        }
    }
    if (read_count != 2 || read_offsets[0] != 0 || read_offsets[1] != 4096 ||
        read_sizes[0] != 4096 || read_sizes[1] > 4096 || read_sizes[1] == 0) {
        printf("[FAIL] unexpected FUSE_READ split: count=%u off0=%llu size0=%u off1=%llu size1=%u\n",
               read_count, (unsigned long long)read_offsets[0], read_sizes[0],
               (unsigned long long)read_offsets[1], read_sizes[1]);
        goto fail;
    }

    free(buf);
    buf = NULL;
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (buf) {
        free(buf);
    }
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_cached_read_pipelines_requests() {
    const char *mp = "/tmp/test_fuse_read_pipeline";
    const size_t data_size = 64 * 1024;
    char *buf = NULL;
    int n = -1;
    int ok = 0;
    if (ensure_dir(mp) != 0)
        return -1;
    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        rmdir(mp);
        return -1;
    }
    volatile int stop = 0, init_done = 0, saw_pipeline = 0;
    volatile uint32_t read_count = 0;
    volatile uint32_t init_in_flags = 0;
    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.stop_on_destroy = 1;
    args.init_in_flags = &init_in_flags;
    args.init_out_flags_override = FUSE_INIT_EXT | FUSE_MAX_PAGES | FUSE_ASYNC_READ;
    args.hello_generated_size_override = data_size;
    args.read_count = &read_count;
    args.defer_first_read_reply = 2;
    args.saw_pipelined_read = &saw_pipeline;
    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0)
        goto fail_no_thread;
    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0 || fuseg_wait_init(&init_done) != 0)
        goto fail;
    char path[256];
    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    buf = (char *)malloc(data_size);
    if (!buf)
        goto fail;
    n = fuseg_read_file(path, buf, 4096);
    ok = n == 4096 && (init_in_flags & FUSE_ASYNC_READ) != 0 && saw_pipeline && read_count >= 2;
    for (size_t i = 0; ok && i < 4096; ++i)
        ok = buf[i] == (char)('A' + (i % 26));
    free(buf);
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return ok ? 0 : -1;
fail:
    free(buf);
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
fail_no_thread:
    close(fd);
    rmdir(mp);
    return -1;
}

struct fuseg_async_read_args {
    const char *path;
    char *buf;
    size_t size;
    int result;
};

static void *fuseg_async_read_thread(void *opaque) {
    struct fuseg_async_read_args *args = (struct fuseg_async_read_args *)opaque;
    args->result = fuseg_read_file(args->path, args->buf, args->size);
    return NULL;
}

static int ext_test_cached_read_without_async_is_serial() {
    const char *mp = "/tmp/test_fuse_read_serial";
    const size_t data_size = 64 * 1024;
    char *buf = NULL;
    int n = -1;
    int ok = 0;
    int wait_rc = 0;
    int mounted = 0, client_started = 0;
    char opts[256], path[256];
    struct fuseg_async_read_args read_args;
    pthread_t th, client_th;
    struct timespec gate_deadline;
    if (ensure_dir(mp) != 0)
        return -1;
    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        rmdir(mp);
        return -1;
    }
    volatile int stop = 0, init_done = 0;
    volatile uint32_t read_count = 0, init_in_flags = 0;
    volatile uint64_t read_offsets[4] = {0};
    pthread_mutex_t first_read_gate_mutex = PTHREAD_MUTEX_INITIALIZER;
    pthread_cond_t first_read_gate_cond = PTHREAD_COND_INITIALIZER;
    int first_read_captured = 0;
    int first_read_gate_state = -1;
    int daemon_waiting_after_first_read = 0;
    int saw_early_read = 0;
    int first_read_reply_result = -9999;
    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.stop_on_destroy = 1;
    args.init_in_flags = &init_in_flags;
    args.init_out_flags_override = FUSE_INIT_EXT | FUSE_MAX_PAGES;
    args.hello_generated_size_override = data_size;
    args.read_count = &read_count;
    args.read_offsets = read_offsets;
    args.read_trace_capacity = 4;
    args.first_read_gate_mutex = &first_read_gate_mutex;
    args.first_read_gate_cond = &first_read_gate_cond;
    args.first_read_captured = &first_read_captured;
    args.first_read_gate_state = &first_read_gate_state;
    args.daemon_waiting_after_first_read = &daemon_waiting_after_first_read;
    args.saw_read_before_first_reply = &saw_early_read;
    args.first_read_reply_result = &first_read_reply_result;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0)
        goto fail_no_thread;
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0)
        goto fail;
    mounted = 1;
    if (fuseg_wait_init(&init_done) != 0)
        goto fail;
    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    buf = (char *)malloc(data_size);
    if (!buf)
        goto fail;
    memset(&read_args, 0, sizeof(read_args));
    read_args.path = path;
    read_args.buf = buf;
    read_args.size = 8192;
    if (pthread_create(&client_th, NULL, fuseg_async_read_thread, &read_args) != 0)
        goto fail;
    client_started = 1;
    clock_gettime(CLOCK_REALTIME, &gate_deadline);
    gate_deadline.tv_sec += 5;
    pthread_mutex_lock(&first_read_gate_mutex);
    wait_rc = 0;
    while (!first_read_captured && wait_rc == 0)
        wait_rc = pthread_cond_timedwait(&first_read_gate_cond, &first_read_gate_mutex,
                                         &gate_deadline);
    if (!first_read_captured) {
        pthread_mutex_unlock(&first_read_gate_mutex);
        goto fail;
    }
    wait_rc = 0;
    while (!daemon_waiting_after_first_read && wait_rc == 0)
        wait_rc = pthread_cond_timedwait(&first_read_gate_cond, &first_read_gate_mutex,
                                         &gate_deadline);
    if (!daemon_waiting_after_first_read) {
        pthread_mutex_unlock(&first_read_gate_mutex);
        goto fail;
    }
    wait_rc = 0;
    while (__atomic_load_n(&first_read_gate_state, __ATOMIC_ACQUIRE) == 0 && wait_rc == 0)
        wait_rc = pthread_cond_timedwait(&first_read_gate_cond, &first_read_gate_mutex,
                                         &gate_deadline);
    ok = __atomic_load_n(&first_read_gate_state, __ATOMIC_ACQUIRE) == 3
         && first_read_reply_result == 0 && !saw_early_read;
    pthread_mutex_unlock(&first_read_gate_mutex);
    if (!ok)
        goto fail;
    pthread_join(client_th, NULL);
    client_started = 0;
    n = read_args.result;
    ok = n == 8192 && (init_in_flags & FUSE_ASYNC_READ) != 0 && !saw_early_read
         && read_count >= 2 && read_offsets[0] == 0 && read_offsets[1] == 4096
         && first_read_reply_result == 0
         && __atomic_load_n(&first_read_gate_state, __ATOMIC_ACQUIRE) == 3 && ok;
    for (size_t i = 0; ok && i < 8192; ++i)
        ok = buf[i] == (char)('A' + (i % 26));
    free(buf);
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return ok ? 0 : -1;
fail:
    stop = 1;
    close(fd);
    if (client_started)
        pthread_join(client_th, NULL);
    free(buf);
    if (mounted)
        umount(mp);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
fail_no_thread:
    close(fd);
    rmdir(mp);
    return -1;
}

static int ext_run_cached_read_sync_error_case(const char *mp, uint64_t error_offset,
                                               int error_once, int case_kind) {
    const size_t data_size = 64 * 1024;
    int result = -1;
    int file_fd = -1;
    ssize_t first = -1, second = -1;
    int first_errno = 0, second_errno = 0;
    char buf[8192];
    if (ensure_dir(mp) != 0)
        return -1;
    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        rmdir(mp);
        return -1;
    }
    volatile int stop = 0, init_done = 0;
    volatile uint32_t init_in_flags = 0, read_count = 0;
    volatile uint64_t read_offsets[8] = {0};
    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.stop_on_destroy = 1;
    args.init_in_flags = &init_in_flags;
    args.init_out_flags_override = FUSE_INIT_EXT | FUSE_MAX_PAGES;
    args.hello_generated_size_override = data_size;
    args.read_count = &read_count;
    args.read_offsets = read_offsets;
    args.read_trace_capacity = 8;
    args.has_forced_read_error = 1;
    args.forced_read_errno = EIO;
    args.forced_read_error_once = error_once;
    args.forced_read_error_offset = error_offset;
    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0)
        goto fail_no_thread;
    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0 || fuseg_wait_init(&init_done) != 0)
        goto fail;
    char path[256];
    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    file_fd = open(path, O_RDONLY);
    if (file_fd < 0 || (init_in_flags & FUSE_ASYNC_READ) == 0)
        goto fail;

    errno = 0;
    first = pread(file_fd, buf, case_kind == 2 ? 4096 : 8192, 0);
    first_errno = errno;
    if (case_kind == 2) {
        result = first == -1 && first_errno == EIO && read_count == 1 && read_offsets[0] == 0 ? 0
                                                                                           : -1;
        goto out;
    }
    if (first != 4096 || read_count != 2 || read_offsets[0] != 0 || read_offsets[1] != 4096)
        goto out;
    for (size_t i = 0; i < 4096; ++i) {
        if (buf[i] != (char)('A' + (i % 26)))
            goto out;
    }
    errno = 0;
    second = pread(file_fd, buf + 4096, 4096, 4096);
    second_errno = errno;
    if (!error_once) {
        result = second == -1 && second_errno == EIO && read_count == 3
                         && read_offsets[2] == 4096
                     ? 0
                     : -1;
    } else {
        result = second == 4096 && read_count == 3 && read_offsets[2] == 4096 ? 0 : -1;
        for (size_t i = 0; result == 0 && i < 4096; ++i) {
            if (buf[4096 + i] != (char)('A' + ((4096 + i) % 26)))
                result = -1;
        }
    }
out:
    if (file_fd >= 0)
        close(file_fd);
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return result;
fail:
    if (file_fd >= 0)
        close(file_fd);
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
fail_no_thread:
    close(fd);
    rmdir(mp);
    return -1;
}

static int ext_test_cached_read_sync_error_semantics() {
    if (ext_run_cached_read_sync_error_case("/tmp/test_fuse_read_eio_persistent", 4096, 0, 0)
        != 0)
        return -1;
    if (ext_run_cached_read_sync_error_case("/tmp/test_fuse_read_eio_once", 4096, 1, 1) != 0)
        return -1;
    return ext_run_cached_read_sync_error_case("/tmp/test_fuse_read_eio_first", 0, 0, 2);
}

static int ext_test_cached_read_uses_open_fh_without_extra_open() {
    const char *mp = "/tmp/test_fuse_cached_read_fh";
    char path[256];
    char buf[32];
    int f = -1;
    ssize_t n = -1;
    ssize_t first_n = -1;

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t open_count = 0;
    volatile uint32_t read_count = 0;
    volatile uint64_t read_fhs[4] = {0};

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.open_count = &open_count;
    args.read_count = &read_count;
    args.read_fhs = read_fhs;
    args.read_trace_capacity = 4;
    args.next_open_fh = 100;
    args.stop_on_destroy = 1;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDONLY);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    n = pread(f, buf, sizeof(buf), 0);
    if (n <= 0) {
        printf("[FAIL] first pread got=%zd errno=%d\n", n, errno);
        close(f);
        goto fail;
    }
    first_n = n;
    memset(buf, 0, sizeof(buf));
    n = pread(f, buf, sizeof(buf), 0);
    close(f);
    f = -1;
    if (n != first_n) {
        printf("[FAIL] second pread got=%zd errno=%d\n", n, errno);
        goto fail;
    }
    if (open_count != 1 || read_count != 1 || read_fhs[0] != 100) {
        printf("[FAIL] cached read counters open=%u read=%u fh0=%llu\n", open_count,
               read_count, (unsigned long long)read_fhs[0]);
        goto fail;
    }

    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_cached_short_read_updates_eof() {
    const char *mp = "/tmp/test_fuse_cached_short_read";
    char path[256];
    char buf[32];
    int f = -1;
    ssize_t n = -1;
    uint32_t reads_after_short = 0;

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t read_count = 0;
    volatile uint64_t read_offsets[4] = {0};
    volatile uint32_t read_sizes[4] = {0};

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.read_count = &read_count;
    args.read_offsets = read_offsets;
    args.read_sizes = read_sizes;
    args.read_trace_capacity = 4;
    args.hello_data_size_override = 8192;
    args.hello_read_size_override = 5;
    args.stop_on_destroy = 1;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDONLY);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }

    memset(buf, 0x7f, sizeof(buf));
    n = pread(f, buf, sizeof(buf), 0);
    if (n != 5 || memcmp(buf, "ABCDE", 5) != 0) {
        printf("[FAIL] short cached pread got=%zd data='%.*s' read=%u errno=%d\n", n, 5, buf,
               read_count, errno);
        goto fail;
    }

    // The second READ is speculative readahead, so the foreground pread must
    // not wait for the daemon to consume it. Wait here before inspecting the
    // asynchronous trace rather than depending on daemon scheduling.
    for (int i = 0;
         i < 200 &&
         (read_count < 2 || read_offsets[1] != 4096 || read_sizes[1] != 4096);
         ++i) {
        usleep(5 * 1000);
    }
    if (read_count < 2 || read_offsets[0] != 0 || read_sizes[0] != 4096 ||
        read_offsets[1] != 4096 || read_sizes[1] != 4096) {
        printf("[FAIL] short read trace count=%u off0=%llu size0=%u off1=%llu size1=%u\n",
               read_count, (unsigned long long)read_offsets[0], read_sizes[0],
               (unsigned long long)read_offsets[1], read_sizes[1]);
        goto fail;
    }
    reads_after_short = read_count;

    memset(buf, 0x7f, sizeof(buf));
    n = pread(f, buf, sizeof(buf), 5);
    if (n != 0) {
        printf("[FAIL] EOF cached pread got=%zd read=%u errno=%d\n", n, read_count, errno);
        goto fail;
    }
    if (read_count != reads_after_short) {
        printf("[FAIL] cached EOF issued a new READ before=%u after=%u\n", reads_after_short,
               read_count);
        goto fail;
    }

    close(f);
    f = -1;
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_short_read_discards_old_pages_after_regrow() {
    const char *mp = "/tmp/test_fuse_short_read_regrow";
    char path[256];
    int f = -1;
    unsigned char byte = 0;
    if (ensure_dir(mp) != 0)
        return -1;
    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0, init_done = 0;
    volatile size_t visible_size = 8192;
    volatile size_t watched_offset = 4096;
    volatile unsigned char backend_byte = 'X';
    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;
    args.hello_data_size_override = 8192;
    args.dynamic_hello_read_size = &visible_size;
    args.dynamic_hello_byte_offset = &watched_offset;
    args.dynamic_hello_byte = &backend_byte;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0)
        goto fail_no_thread;
    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0 || fuseg_wait_init(&init_done) != 0)
        goto fail;
    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDWR);
    if (f < 0)
        goto fail;

    if (pread(f, &byte, 1, 4096) != 1 || byte != 'X')
        goto fail;

    backend_byte = 'Y';
    visible_size = 5;
    if (pread(f, &byte, 1, 0) != 1 || byte != 'A')
        goto fail;

    visible_size = 8192;
    if (ftruncate(f, 8192) != 0)
        goto fail;
    byte = 0;
    if (pread(f, &byte, 1, 4096) != 1 || byte != 'Y') {
        printf("[FAIL] regrown read returned stale byte=%u expected=%u errno=%d\n", byte, 'Y',
               errno);
        goto fail;
    }

    close(f);
    f = -1;
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;
fail:
    if (f >= 0)
        close(f);
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
fail_no_thread:
    close(fd);
    rmdir(mp);
    return -1;
}

static int ext_test_cached_read_sees_write_through_update() {
    const char *mp = "/tmp/test_fuse_cached_read_write";
    char path[256];
    char buf[16];
    int f = -1;

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t open_count = 0;
    volatile uint32_t read_count = 0;
    volatile uint32_t write_count = 0;
    volatile uint64_t last_write_fh = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.open_count = &open_count;
    args.read_count = &read_count;
    args.write_count = &write_count;
    args.last_write_fh = &last_write_fh;
    args.next_open_fh = 300;
    args.stop_on_destroy = 1;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDWR);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    memset(buf, 0, sizeof(buf));
    if (pread(f, buf, 5, 0) != 5 || memcmp(buf, "hello", 5) != 0) {
        printf("[FAIL] first cached pread got='%.*s' read=%u errno=%d\n", 5, buf, read_count,
               errno);
        goto fail;
    }
    if (pwrite(f, "CACHE", 5, 0) != 5) {
        printf("[FAIL] pwrite CACHE: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    memset(buf, 0, sizeof(buf));
    if (pread(f, buf, 5, 0) != 5 || memcmp(buf, "CACHE", 5) != 0) {
        printf("[FAIL] second cached pread got='%.*s' read=%u errno=%d\n", 5, buf, read_count,
               errno);
        goto fail;
    }
    if (open_count != 1 || read_count != 1 || write_count != 1 || last_write_fh != 300) {
        printf("[FAIL] cached write counters open=%u read=%u write=%u wfh=%llu\n", open_count,
               read_count, write_count, (unsigned long long)last_write_fh);
        goto fail;
    }

    close(f);
    f = -1;
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_mmap_sees_write_through_update() {
    const char *mp = "/tmp/test_fuse_mmap_write_through";
    char path[256];
    char buf[16];
    int f = -1;
    void *addr = MAP_FAILED;
    pid_t child = -1;
    struct mmap_write_shared_state {
        volatile int stop;
        volatile int init_done;
        volatile uint32_t open_count;
        volatile uint32_t read_count;
        volatile uint32_t write_count;
        volatile uint64_t last_write_fh;
        volatile uint64_t read_fhs[4];
    };
    struct mmap_write_shared_state *shared =
        (struct mmap_write_shared_state *)mmap(NULL, sizeof(*shared), PROT_READ | PROT_WRITE,
                                               MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (shared == MAP_FAILED) {
        printf("[FAIL] mmap(shared counters): %s (errno=%d)\n", strerror(errno), errno);
        return -1;
    }
    memset(shared, 0, sizeof(*shared));

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }

    child = fork();
    if (child < 0) {
        printf("[FAIL] fork fuse daemon: %s (errno=%d)\n", strerror(errno), errno);
        close(fd);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (child == 0) {
        struct fuse_daemon_args child_args;
        memset(&child_args, 0, sizeof(child_args));
        child_args.fd = fd;
        child_args.stop = &shared->stop;
        child_args.init_done = &shared->init_done;
        child_args.stop_on_destroy = 1;
        child_args.enable_write_ops = 1;
        child_args.open_count = &shared->open_count;
        child_args.read_count = &shared->read_count;
        child_args.write_count = &shared->write_count;
        child_args.last_write_fh = &shared->last_write_fh;
        child_args.read_fhs = shared->read_fhs;
        child_args.read_trace_capacity = 4;
        child_args.next_open_fh = 320;
        fuse_daemon_thread(&child_args);
        _exit(0);
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        shared->stop = 1;
        close(fd);
        kill(child, SIGTERM);
        waitpid(child, NULL, 0);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&shared->init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDWR);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    addr = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, f, 0);
    if (addr == MAP_FAILED) {
        printf("[FAIL] mmap(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    if (((volatile char *)addr)[0] != 'h') {
        printf("[FAIL] mmap warmup first byte got=%d\n", ((volatile char *)addr)[0]);
        goto fail;
    }
    if (pwrite(f, "MMAP!", 5, 0) != 5) {
        printf("[FAIL] pwrite MMAP!: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (memcmp(addr, "MMAP!", 5) != 0) {
        printf("[FAIL] mmap page did not observe write-through update, got='%.*s'\n", 5,
               (char *)addr);
        goto fail;
    }
    memset(buf, 0, sizeof(buf));
    if (pread(f, buf, 5, 0) != 5 || memcmp(buf, "MMAP!", 5) != 0) {
        printf("[FAIL] cached pread after mmap write got='%.*s' read=%u errno=%d\n", 5, buf,
               shared->read_count, errno);
        goto fail;
    }
    if (shared->open_count != 1 || shared->read_count != 1 || shared->write_count != 1 ||
        shared->last_write_fh != 320 || shared->read_fhs[0] != 320) {
        printf("[FAIL] mmap write-through counters open=%u read=%u write=%u rfh=%llu wfh=%llu\n",
               shared->open_count, shared->read_count, shared->write_count,
               (unsigned long long)shared->read_fhs[0],
               (unsigned long long)shared->last_write_fh);
        goto fail;
    }

    munmap(addr, 4096);
    addr = MAP_FAILED;
    close(f);
    f = -1;
    umount(mp);
    shared->stop = 1;
    close(fd);
    waitpid(child, NULL, 0);
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return 0;

fail:
    if (addr != MAP_FAILED) {
        munmap(addr, 4096);
    }
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    shared->stop = 1;
    close(fd);
    if (child > 0) {
        kill(child, SIGTERM);
        waitpid(child, NULL, 0);
    }
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return -1;
}

static int ext_test_mmap_fault_uses_open_fh_without_extra_open() {
    const char *mp = "/tmp/test_fuse_mmap_fh";
    char path[256];
    int f = -1;
    void *addr = MAP_FAILED;
    volatile char c = 0;
    pid_t child = -1;
    struct mmap_shared_state {
        volatile int stop;
        volatile int init_done;
        volatile uint32_t open_count;
        volatile uint32_t read_count;
        volatile uint64_t read_fhs[4];
    };
    struct mmap_shared_state *shared =
        (struct mmap_shared_state *)mmap(NULL, sizeof(*shared), PROT_READ | PROT_WRITE,
                                         MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (shared == MAP_FAILED) {
        printf("[FAIL] mmap(shared counters): %s (errno=%d)\n", strerror(errno), errno);
        return -1;
    }
    memset(shared, 0, sizeof(*shared));

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }

    child = fork();
    if (child < 0) {
        printf("[FAIL] fork fuse daemon: %s (errno=%d)\n", strerror(errno), errno);
        close(fd);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (child == 0) {
        struct fuse_daemon_args child_args;
        memset(&child_args, 0, sizeof(child_args));
        child_args.fd = fd;
        child_args.stop = &shared->stop;
        child_args.init_done = &shared->init_done;
        child_args.stop_on_destroy = 1;
        child_args.open_count = &shared->open_count;
        child_args.read_count = &shared->read_count;
        child_args.read_fhs = shared->read_fhs;
        child_args.read_trace_capacity = 4;
        child_args.next_open_fh = 200;
        fuse_daemon_thread(&child_args);
        _exit(0);
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        shared->stop = 1;
        close(fd);
        kill(child, SIGTERM);
        waitpid(child, NULL, 0);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&shared->init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }
    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDONLY);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    addr = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, f, 0);
    if (addr == MAP_FAILED) {
        printf("[FAIL] mmap(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        close(f);
        goto fail;
    }
    c = ((volatile char *)addr)[0];
    if (c != 'h') {
        printf("[FAIL] mmap first byte got=%d\n", c);
        munmap(addr, 4096);
        close(f);
        goto fail;
    }
    munmap(addr, 4096);
    addr = MAP_FAILED;
    close(f);
    f = -1;

    if (shared->open_count != 1 || shared->read_count != 1 || shared->read_fhs[0] != 200) {
        printf("[FAIL] mmap counters open=%u read=%u fh0=%llu\n", shared->open_count,
               shared->read_count, (unsigned long long)shared->read_fhs[0]);
        goto fail;
    }

    umount(mp);
    shared->stop = 1;
    close(fd);
    waitpid(child, NULL, 0);
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return 0;

fail:
    if (addr != MAP_FAILED) {
        munmap(addr, 4096);
    }
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    shared->stop = 1;
    close(fd);
    if (child > 0) {
        kill(child, SIGTERM);
        waitpid(child, NULL, 0);
    }
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return -1;
}

static int ext_test_mmap_fault_batches_readaround_pages() {
    const char *mp = "/tmp/test_fuse_mmap_readaround";
    const size_t page_size = 4096;
    const size_t page_count = 8;
    const size_t map_len = page_size * page_count;
    char path[256];
    int f = -1;
    void *addr = MAP_FAILED;
    volatile unsigned int checksum = 0;

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t read_count = 0;
    volatile uint64_t read_offsets[8] = {0};
    volatile uint32_t read_sizes[8] = {0};

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.read_count = &read_count;
    args.read_offsets = read_offsets;
    args.read_sizes = read_sizes;
    args.read_trace_capacity = 8;
    args.hello_generated_size_override = map_len;
    args.init_out_max_write_override = map_len;
    args.stop_on_destroy = 1;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=32768",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDONLY);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    addr = mmap(NULL, map_len, PROT_READ, MAP_PRIVATE, f, 0);
    if (addr == MAP_FAILED) {
        printf("[FAIL] mmap(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }

    for (size_t i = 0; i < page_count; i++) {
        size_t offset = i * page_size;
        unsigned char c = ((volatile unsigned char *)addr)[offset];
        unsigned char expected = (unsigned char)('A' + (offset % 26));
        if (c != expected) {
            printf("[FAIL] mmap data mismatch page=%zu got=%u expected=%u read_count=%u\n", i, c,
                   expected, read_count);
            goto fail;
        }
        checksum += c;
    }
    if (checksum == 0) {
        printf("[FAIL] checksum unexpectedly zero\n");
        goto fail;
    }

    if (read_count != 1 || read_offsets[0] != 0 || read_sizes[0] != map_len) {
        printf("[FAIL] mmap readaround not batched: count=%u off0=%llu size0=%u off1=%llu size1=%u\n",
               read_count, (unsigned long long)read_offsets[0], read_sizes[0],
               (unsigned long long)read_offsets[1], read_sizes[1]);
        goto fail;
    }

    munmap(addr, map_len);
    addr = MAP_FAILED;
    close(f);
    f = -1;
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (addr != MAP_FAILED) {
        munmap(addr, map_len);
    }
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_direct_io_read_bypasses_page_cache() {
    const char *mp = "/tmp/test_fuse_direct_read";
    char path[256];
    char buf[32];
    int f = -1;

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t open_count = 0;
    volatile uint32_t read_count = 0;
    volatile uint64_t read_fhs[4] = {0};

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.open_count = &open_count;
    args.read_count = &read_count;
    args.read_fhs = read_fhs;
    args.read_trace_capacity = 4;
    args.next_open_fh = 700;
    args.hello_open_out_flags = FOPEN_DIRECT_IO;
    args.stop_on_destroy = 1;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDONLY);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    memset(buf, 0, sizeof(buf));
    if (pread(f, buf, 5, 0) != 5 || memcmp(buf, "hello", 5) != 0) {
        printf("[FAIL] first direct pread got='%.*s' read=%u errno=%d\n", 5, buf, read_count,
               errno);
        goto fail;
    }
    memset(buf, 0, sizeof(buf));
    if (pread(f, buf, 5, 0) != 5 || memcmp(buf, "hello", 5) != 0) {
        printf("[FAIL] second direct pread got='%.*s' read=%u errno=%d\n", 5, buf, read_count,
               errno);
        goto fail;
    }
    close(f);
    f = -1;

    if (open_count != 1 || read_count != 2 || read_fhs[0] != 700 || read_fhs[1] != 700) {
        printf("[FAIL] direct read counters open=%u read=%u fh0=%llu fh1=%llu\n", open_count,
               read_count, (unsigned long long)read_fhs[0], (unsigned long long)read_fhs[1]);
        goto fail;
    }

    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_direct_io_write_invalidates_cached_read() {
    const char *mp = "/tmp/test_fuse_direct_write_inval";
    char path[256];
    char buf[16];
    int cached_fd = -1;
    int direct_fd = -1;

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t open_count = 0;
    volatile uint32_t read_count = 0;
    volatile uint32_t write_count = 0;
    volatile uint32_t open_out_flags = 0;
    volatile uint64_t last_write_fh = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.open_count = &open_count;
    args.read_count = &read_count;
    args.write_count = &write_count;
    args.dynamic_hello_open_out_flags = &open_out_flags;
    args.last_write_fh = &last_write_fh;
    args.next_open_fh = 520;
    args.stop_on_destroy = 1;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    cached_fd = open(path, O_RDWR);
    if (cached_fd < 0) {
        printf("[FAIL] open cached fd: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    memset(buf, 0, sizeof(buf));
    if (pread(cached_fd, buf, 5, 0) != 5 || memcmp(buf, "hello", 5) != 0) {
        printf("[FAIL] initial cached pread got='%.*s' read=%u errno=%d\n", 5, buf, read_count,
               errno);
        goto fail;
    }

    open_out_flags = FOPEN_DIRECT_IO;
    direct_fd = open(path, O_WRONLY);
    if (direct_fd < 0) {
        printf("[FAIL] open direct fd: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (pwrite(direct_fd, "DIO!!", 5, 0) != 5) {
        printf("[FAIL] direct pwrite: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (pwrite(direct_fd, "TAIL!", 5, 20) != 5) {
        printf("[FAIL] direct pwrite extend: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    close(direct_fd);
    direct_fd = -1;
    open_out_flags = 0;

    memset(buf, 0, sizeof(buf));
    if (pread(cached_fd, buf, 5, 0) != 5 || memcmp(buf, "DIO!!", 5) != 0) {
        printf("[FAIL] cached pread after direct write got='%.*s' read=%u errno=%d\n", 5, buf,
               read_count, errno);
        goto fail;
    }
    memset(buf, 0, sizeof(buf));
    if (pread(cached_fd, buf, 5, 20) != 5 || memcmp(buf, "TAIL!", 5) != 0) {
        printf("[FAIL] cached pread after direct extend got='%.*s' read=%u errno=%d\n", 5, buf,
               read_count, errno);
        goto fail;
    }
    if (open_count != 2 || read_count != 2 || write_count != 2 || last_write_fh != 521) {
        printf("[FAIL] direct write counters open=%u read=%u write=%u wfh=%llu\n", open_count,
               read_count, write_count, (unsigned long long)last_write_fh);
        goto fail;
    }

    close(cached_fd);
    cached_fd = -1;
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (direct_fd >= 0) {
        close(direct_fd);
    }
    if (cached_fd >= 0) {
        close(cached_fd);
    }
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_direct_io_mmap_policy() {
    const char *mp = "/tmp/test_fuse_direct_mmap";
    char path[256];
    int f = -1;
    void *addr = MAP_FAILED;
    volatile char c = 0;
    char warm = 0;
    pid_t child = -1;
    struct direct_mmap_shared_state {
        volatile int stop;
        volatile int init_done;
        volatile uint32_t open_out_flags;
        volatile unsigned char first_byte;
        volatile uint32_t open_count;
        volatile uint32_t read_count;
        volatile uint64_t read_fhs[4];
    };
    struct direct_mmap_shared_state *shared =
        (struct direct_mmap_shared_state *)mmap(NULL, sizeof(*shared), PROT_READ | PROT_WRITE,
                                                MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (shared == MAP_FAILED) {
        printf("[FAIL] mmap(shared counters): %s (errno=%d)\n", strerror(errno), errno);
        return -1;
    }
    memset(shared, 0, sizeof(*shared));

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }

    child = fork();
    if (child < 0) {
        printf("[FAIL] fork fuse daemon: %s (errno=%d)\n", strerror(errno), errno);
        close(fd);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (child == 0) {
        struct fuse_daemon_args child_args;
        memset(&child_args, 0, sizeof(child_args));
        child_args.fd = fd;
        child_args.stop = &shared->stop;
        child_args.init_done = &shared->init_done;
        child_args.stop_on_destroy = 1;
        child_args.open_count = &shared->open_count;
        child_args.read_count = &shared->read_count;
        child_args.read_fhs = shared->read_fhs;
        child_args.read_trace_capacity = 4;
        child_args.next_open_fh = 800;
        child_args.dynamic_hello_open_out_flags = &shared->open_out_flags;
        child_args.dynamic_hello_first_byte = &shared->first_byte;
        fuse_daemon_thread(&child_args);
        _exit(0);
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        shared->stop = 1;
        close(fd);
        kill(child, SIGTERM);
        waitpid(child, NULL, 0);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&shared->init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDONLY);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    if (pread(f, &warm, 1, 0) != 1 || warm != 'h') {
        printf("[FAIL] warm cached read got=%d read=%u errno=%d\n", warm, shared->read_count,
               errno);
        goto fail;
    }
    close(f);
    f = -1;

    shared->open_out_flags = FOPEN_DIRECT_IO;
    shared->first_byte = 'Z';

    f = open(path, O_RDONLY);
    if (f < 0) {
        printf("[FAIL] direct open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }

    errno = 0;
    addr = mmap(NULL, 4096, PROT_READ, MAP_SHARED, f, 0);
    if (addr != MAP_FAILED) {
        printf("[FAIL] direct_io MAP_SHARED unexpectedly succeeded\n");
        munmap(addr, 4096);
        addr = MAP_FAILED;
        goto fail;
    }
    if (errno != ENODEV) {
        printf("[FAIL] direct_io MAP_SHARED errno=%d expected=%d\n", errno, ENODEV);
        goto fail;
    }

    addr = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, f, 0);
    if (addr == MAP_FAILED) {
        printf("[FAIL] direct_io MAP_PRIVATE mmap: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    c = ((volatile char *)addr)[0];
    if (c != 'Z') {
        printf("[FAIL] direct_io MAP_PRIVATE first byte got=%d\n", c);
        goto fail;
    }
    if (shared->open_count != 2 || shared->read_count != 2 || shared->read_fhs[1] != 801) {
        printf("[FAIL] direct mmap counters open=%u read=%u fh0=%llu fh1=%llu\n",
               shared->open_count, shared->read_count, (unsigned long long)shared->read_fhs[0],
               (unsigned long long)shared->read_fhs[1]);
        goto fail;
    }

    munmap(addr, 4096);
    addr = MAP_FAILED;
    close(f);
    f = -1;
    umount(mp);
    shared->stop = 1;
    close(fd);
    waitpid(child, NULL, 0);
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return 0;

fail:
    if (addr != MAP_FAILED) {
        munmap(addr, 4096);
    }
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    shared->stop = 1;
    close(fd);
    if (child > 0) {
        kill(child, SIGTERM);
        waitpid(child, NULL, 0);
    }
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return -1;
}

static int ext_test_mmap_writeback_retry_outside_mm_guard_inner(volatile int *stage) {
    char mp[128];
    char debug_root[128];
    char path[256];
    char stats_path[256];
    int fd = -1;
    int f = -1;
    void *addr = MAP_FAILED;
    int mounted = 0;
    int debug_mounted = 0;
    pthread_t daemon_thread;
    pthread_t fsync_thread;
    pthread_t writer_thread;
    int daemon_active = 0;
    int fsync_active = 0;
    int writer_active = 0;
    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint64_t last_create_nodeid = 0;
    volatile uint64_t large_write_nodeid = 0;
    volatile uint32_t write_count = 0;
    volatile uint64_t write_offsets[8] = {0};
    volatile uint32_t write_sizes[8] = {0};
    volatile uint32_t write_flags[8] = {0};
    volatile uint64_t block_write_offset = 0;
    volatile int write_entered = 0;
    volatile int release_write = 0;
    uint64_t retry_count_before = 0;
    uint64_t retry_count_after = 0;
    unsigned char backing[8192];
    unsigned char payload[8192];
    struct fuse_fsync_worker_args fsync_args;
    struct fuse_mmap_single_write_args writer_args;
    memset(backing, 0, sizeof(backing));
    for (size_t i = 0; i < sizeof(payload); ++i) {
        payload[i] = (unsigned char)(i * 13u + 7u);
    }
    memset(&fsync_args, 0, sizeof(fsync_args));
    memset(&writer_args, 0, sizeof(writer_args));
    snprintf(mp, sizeof(mp), "/tmp/test_fuse_mmap_writeback_retry_%d", getpid());
    snprintf(debug_root, sizeof(debug_root), "/tmp/test_fuse_mmap_writeback_debugfs_%d",
             getpid());
    snprintf(stats_path, sizeof(stats_path), "%s/fuse/stats", debug_root);

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }
    if (ensure_dir(debug_root) != 0 || mount("none", debug_root, "debugfs", 0, NULL) != 0) {
        printf("[FAIL] mount mmap retry debugfs: %s (errno=%d)\n", strerror(errno), errno);
        rmdir(debug_root);
        rmdir(mp);
        return -1;
    }
    debug_mounted = 1;
    if (fuse_count_mounted_filesystems() != 0) {
        printf("[FAIL] mmap retry test requires an isolated guest with no FUSE mount\n");
        goto fail;
    }
    if (fuse_read_u64_counter(stats_path, "mmap_writeback_admission_retries_total",
                              &retry_count_before) != 0) {
        printf("[FAIL] read mmap admission retry counter before test\n");
        goto fail;
    }
    fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    struct fuse_daemon_args daemon_args;
    memset(&daemon_args, 0, sizeof(daemon_args));
    daemon_args.fd = fd;
    daemon_args.stop = &stop;
    daemon_args.init_done = &init_done;
    daemon_args.enable_write_ops = 1;
    daemon_args.stop_on_destroy = 1;
    daemon_args.last_create_nodeid = &last_create_nodeid;
    daemon_args.write_count = &write_count;
    daemon_args.write_offsets = write_offsets;
    daemon_args.write_sizes = write_sizes;
    daemon_args.write_flags = write_flags;
    daemon_args.write_trace_capacity = 8;
    daemon_args.large_write_backing = backing;
    daemon_args.large_write_backing_capacity = sizeof(backing);
    daemon_args.large_write_nodeid = &large_write_nodeid;
    daemon_args.block_write_offset = &block_write_offset;
    daemon_args.write_entered = &write_entered;
    daemon_args.release_write = &release_write;
    daemon_args.init_out_flags_override = FUSE_INIT_EXT | FUSE_MAX_PAGES | FUSE_WRITEBACK_CACHE;
    daemon_args.init_out_max_write_override = 4096;
    if (pthread_create(&daemon_thread, NULL, fuse_daemon_thread, &daemon_args) != 0) {
        printf("[FAIL] pthread_create fuse daemon\n");
        goto fail;
    }
    daemon_active = 1;

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount/init mmap retry daemon: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    mounted = 1;
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] mmap retry init handshake timeout\n");
        goto fail;
    }
    if (fuse_count_mounted_filesystems() != 1) {
        printf("[FAIL] mmap retry test lost exclusive FUSE mount ownership\n");
        goto fail;
    }
    *stage = 1;
    __sync_synchronize();
    snprintf(path, sizeof(path), "%s/two_pages", mp);
    f = open(path, O_CREAT | O_RDWR, 0644);
    if (f < 0 || write(f, payload, sizeof(payload)) != (ssize_t)sizeof(payload)) {
        printf("[FAIL] create two-page mmap retry file: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    large_write_nodeid = last_create_nodeid;
    if (large_write_nodeid == 0 || write_count != 0) {
        printf("[FAIL] writeback-cache setup node=%llu writes=%u\n",
               (unsigned long long)large_write_nodeid, write_count);
        goto fail;
    }
    addr = mmap(NULL, sizeof(payload), PROT_READ | PROT_WRITE, MAP_SHARED, f, 0);
    if (addr == MAP_FAILED || ((volatile unsigned char *)addr)[4096] != payload[4096]) {
        printf("[FAIL] prefault second mmap page: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    fsync_args.fd = f;
    if (pthread_create(&fsync_thread, NULL, fuse_fsync_worker, &fsync_args) != 0) {
        printf("[FAIL] pthread_create fsync worker\n");
        goto fail;
    }
    fsync_active = 1;
    if (fuse_wait_flag(&write_entered, 5000) != 0 || write_count != 0) {
        printf("[FAIL] first split WRITE did not enter blocking daemon entered=%d count=%u\n",
               write_entered, write_count);
        goto fail;
    }
    *stage = 2;
    __sync_synchronize();

    writer_args.address = (volatile char *)addr + 4096;
    writer_args.value = 'Z';
    if (pthread_create(&writer_thread, NULL, fuse_mmap_single_write_worker, &writer_args) != 0) {
        printf("[FAIL] pthread_create mmap writer\n");
        goto fail;
    }
    writer_active = 1;
    if (fuse_wait_flag(&writer_args.started, 1000) != 0) {
        printf("[FAIL] mmap writer did not start\n");
        goto fail;
    }
    *stage = 3;
    __sync_synchronize();
    if (fuse_wait_counter_increase(stats_path, "mmap_writeback_admission_retries_total",
                                   retry_count_before, &retry_count_after, 5000) != 0 ||
        writer_args.done) {
        printf("[FAIL] mmap writer did not enter admission retry count=%llu->%llu done=%d\n",
               (unsigned long long)retry_count_before,
               (unsigned long long)retry_count_after, writer_args.done);
        goto fail;
    }
    *stage = 4;
    __sync_synchronize();

    release_write = 1;
    __sync_synchronize();
    *stage = 5;
    __sync_synchronize();
    if (fuse_wait_flag(&fsync_args.done, 5000) != 0) {
        *stage = 6;
        __sync_synchronize();
        printf("[FAIL] mmap/fsync retry did not complete fsync=%d writer=%d\n",
               fsync_args.done, writer_args.done);
        goto fail;
    }
    *stage = 7;
    __sync_synchronize();
    if (fuse_wait_flag(&writer_args.done, 5000) != 0) {
        *stage = 8;
        __sync_synchronize();
        printf("[FAIL] mmap/fsync retry did not complete fsync=%d writer=%d\n",
               fsync_args.done, writer_args.done);
        goto fail;
    }
    *stage = 9;
    __sync_synchronize();
    pthread_join(fsync_thread, NULL);
    fsync_active = 0;
    pthread_join(writer_thread, NULL);
    writer_active = 0;
    if (fsync_args.result != 0 || fsync(f) != 0) {
        printf("[FAIL] mmap retry fsync results first=%d/%d final=%d\n", fsync_args.result,
               fsync_args.saved_errno, errno);
        goto fail;
    }
    payload[4096] = 'Z';
    if (write_count != 3 || write_offsets[0] != 0 || write_offsets[1] != 4096 ||
        write_offsets[2] != 4096 || write_sizes[0] != 4096 || write_sizes[1] != 4096 ||
        write_sizes[2] != 4096 || write_flags[0] != FUSE_WRITE_CACHE ||
        write_flags[1] != FUSE_WRITE_CACHE || write_flags[2] != FUSE_WRITE_CACHE ||
        memcmp(backing, payload, sizeof(payload)) != 0) {
        printf("[FAIL] mmap retry trace count=%u writes=(%llu,%u,0x%x),(%llu,%u,0x%x),(%llu,%u,0x%x)\n",
               write_count, (unsigned long long)write_offsets[0], write_sizes[0], write_flags[0],
               (unsigned long long)write_offsets[1], write_sizes[1], write_flags[1],
               (unsigned long long)write_offsets[2], write_sizes[2], write_flags[2]);
        goto fail;
    }

    munmap(addr, sizeof(payload));
    addr = MAP_FAILED;
    close(f);
    f = -1;
    if (umount(mp) != 0) {
        printf("[FAIL] umount mmap retry mount: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    mounted = 0;
    stop = 1;
    close(fd);
    fd = -1;
    pthread_join(daemon_thread, NULL);
    daemon_active = 0;
    if (umount(debug_root) != 0) {
        printf("[FAIL] umount mmap retry debugfs: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    debug_mounted = 0;
    rmdir(debug_root);
    rmdir(mp);
    return 0;

fail:
    release_write = 1;
    __sync_synchronize();
    if (fsync_active) {
        pthread_join(fsync_thread, NULL);
    }
    if (writer_active) {
        pthread_join(writer_thread, NULL);
    }
    if (addr != MAP_FAILED) {
        munmap(addr, sizeof(payload));
    }
    if (f >= 0) {
        close(f);
    }
    if (mounted) {
        umount(mp);
    }
    stop = 1;
    if (fd >= 0) {
        close(fd);
    }
    if (daemon_active) {
        pthread_join(daemon_thread, NULL);
    }
    if (debug_mounted) {
        umount(debug_root);
    }
    rmdir(debug_root);
    rmdir(mp);
    return -1;
}

static int ext_test_mmap_writeback_retry_outside_mm_guard() {
    char mp[128];
    char debug_root[128];
    volatile int *done = fuse_alloc_child_done();
    if (!done) {
        printf("[FAIL] allocate mmap retry child status: %s (errno=%d)\n", strerror(errno),
               errno);
        return -1;
    }
    volatile int *stage = fuse_alloc_child_done();
    if (!stage) {
        printf("[FAIL] allocate mmap retry stage: %s (errno=%d)\n", strerror(errno), errno);
        munmap((void *)done, sizeof(int));
        return -1;
    }
    pid_t child = fork();
    if (child < 0) {
        printf("[FAIL] fork mmap retry test: %s (errno=%d)\n", strerror(errno), errno);
        munmap((void *)stage, sizeof(int));
        munmap((void *)done, sizeof(int));
        return -1;
    }
    if (child == 0) {
        int result = ext_test_mmap_writeback_retry_outside_mm_guard_inner(stage);
        fflush(NULL);
        fuse_publish_child_done(done);
        _exit(result == 0 ? 0 : 1);
    }
    snprintf(mp, sizeof(mp), "/tmp/test_fuse_mmap_writeback_retry_%d", child);
    snprintf(debug_root, sizeof(debug_root), "/tmp/test_fuse_mmap_writeback_debugfs_%d", child);

    int status = 0;
    int waited = fuse_reap_child_bounded(child, done, &status, 15000);
    munmap((void *)done, sizeof(int));
    int result = -1;
    if (waited == 1) {
        result = WIFEXITED(status) && WEXITSTATUS(status) == 0 ? 0 : -1;
    } else {
        int failed_stage = *stage;
        fprintf(stderr, waited == 0 ? "[FAIL] mmap retry test timed out at stage=%d\n"
                                    : "[FAIL] waitpid mmap retry child failed at stage=%d\n",
                failed_stage);
        fflush(stderr);
        munmap((void *)stage, sizeof(int));
        // The timed-out child owned the daemon. Issuing mount cleanup after
        // killing it can itself enqueue FUSE requests with no consumer, hiding
        // the original stage behind a second hang. A failed isolated run is
        // intentionally left for the fresh-guest harness to discard.
        return -100 - failed_stage;
    }
    munmap((void *)stage, sizeof(int));
    if (fuse_cleanup_mounts_bounded(mp, debug_root) != 0) {
        printf("[FAIL] bounded cleanup of mmap retry mounts failed\n");
        result = -1;
    }
    return result;
}

static int ext_test_beyond_eof_dirty_page_retires_inner() {
    char mp[128];
    char path[256];
    int fuse_fd = -1;
    int file_fd = -1;
    int mounted = 0;
    int daemon_active = 0;
    int fsync_result = -1;
    unsigned char original = 0;
    volatile unsigned char *bytes = NULL;
    void *mapping = MAP_FAILED;
    pthread_t daemon_thread;
    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t write_count = 0;
    volatile uint64_t getattr_size_override = UINT64_MAX;
    struct stat st;

    snprintf(mp, sizeof(mp), "/tmp/test_fuse_beyond_eof_dirty_%d", getpid());
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }
    fuse_fd = open("/dev/fuse", O_RDWR);
    if (fuse_fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fuse_fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;
    args.write_count = &write_count;
    args.getattr_size_override = &getattr_size_override;
    args.hello_generated_size_override = 2 * 4096;
    // Deliberately omit FUSE_WRITEBACK_CACHE: a fresh daemon GETATTR may then
    // shrink i_size below an already dirtied mmap page.
    args.init_out_flags_override = FUSE_INIT_EXT | FUSE_MAX_PAGES;
    args.init_out_max_write_override = 8192;
    if (pthread_create(&daemon_thread, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create beyond-EOF daemon\n");
        goto fail;
    }
    daemon_active = 1;

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fuse_fd);
    if (mount("none", mp, "fuse", 0, opts) != 0 || fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] mount/init beyond-EOF daemon: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    mounted = 1;
    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    file_fd = open(path, O_RDWR);
    if (file_fd < 0) {
        printf("[FAIL] open beyond-EOF file: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    mapping = mmap(NULL, 2 * 4096, PROT_READ | PROT_WRITE, MAP_SHARED, file_fd, 0);
    if (mapping == MAP_FAILED) {
        printf("[FAIL] mmap beyond-EOF file: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    bytes = (volatile unsigned char *)mapping;
    original = bytes[4096];
    bytes[4096] = (unsigned char)(original ^ 0x5a);
    __sync_synchronize();

    getattr_size_override = 0;
    __sync_synchronize();
    if (fstat(file_fd, &st) != 0 || st.st_size != 0) {
        printf("[FAIL] GETATTR did not publish daemon shrink size=%lld errno=%d\n",
               (long long)st.st_size, errno);
        goto fail;
    }
    fsync_result = fsync(file_fd);
    if (fsync_result != 0 || write_count != 0) {
        printf("[FAIL] beyond-EOF dirty retirement fsync/write_count=%d/%u errno=%d\n",
               fsync_result, write_count, errno);
        goto fail;
    }

    munmap(mapping, 2 * 4096);
    mapping = MAP_FAILED;
    close(file_fd);
    file_fd = -1;
    if (umount(mp) != 0) {
        printf("[FAIL] umount beyond-EOF mount: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    mounted = 0;
    stop = 1;
    close(fuse_fd);
    fuse_fd = -1;
    pthread_join(daemon_thread, NULL);
    daemon_active = 0;
    rmdir(mp);
    return 0;

fail:
    if (mapping != MAP_FAILED)
        munmap(mapping, 2 * 4096);
    if (file_fd >= 0)
        close(file_fd);
    if (mounted)
        umount(mp);
    stop = 1;
    if (fuse_fd >= 0)
        close(fuse_fd);
    if (daemon_active)
        pthread_join(daemon_thread, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_beyond_eof_dirty_page_retires() {
    char mp[128];
    volatile int *done = fuse_alloc_child_done();
    if (!done) {
        printf("[FAIL] allocate beyond-EOF child status: %s (errno=%d)\n", strerror(errno),
               errno);
        return -1;
    }
    pid_t child = fork();
    if (child < 0) {
        printf("[FAIL] fork beyond-EOF test: %s (errno=%d)\n", strerror(errno), errno);
        munmap((void *)done, sizeof(int));
        return -1;
    }
    if (child == 0) {
        int result = ext_test_beyond_eof_dirty_page_retires_inner();
        fflush(NULL);
        fuse_publish_child_done(done);
        _exit(result == 0 ? 0 : 1);
    }
    snprintf(mp, sizeof(mp), "/tmp/test_fuse_beyond_eof_dirty_%d", child);
    int status = 0;
    int waited = fuse_reap_child_bounded(child, done, &status, 10000);
    munmap((void *)done, sizeof(int));
    int result = waited == 1 && WIFEXITED(status) && WEXITSTATUS(status) == 0 ? 0 : -1;
    if (waited != 1) {
        printf(waited == 0 ? "[FAIL] beyond-EOF dirty test timed out\n"
                           : "[FAIL] waitpid beyond-EOF dirty child failed\n");
    }
    if (fuse_cleanup_mounts_bounded(mp, mp) != 0) {
        printf("[FAIL] bounded cleanup of beyond-EOF dirty mount failed\n");
        result = -1;
    }
    return result;
}

static int ext_test_shared_writable_mmap_msync_writeback() {
    const char *mp = "/tmp/test_fuse_mmap_shared_write";
    char path[256];
    int f = -1;
    void *addr = MAP_FAILED;
    volatile char c = 0;
    pid_t daemon = -1;
    const uint32_t expected_writeback_flags = FUSE_WRITE_CACHE;
    struct mmap_shared_state {
        volatile int stop;
        volatile int init_done;
        volatile uint32_t open_count;
        volatile uint32_t read_count;
        volatile uint32_t write_count;
        volatile uint64_t last_write_fh;
        volatile uint32_t last_open_pid;
        volatile uint64_t last_write_offset;
        volatile uint32_t last_write_size;
        volatile uint32_t last_write_flags;
        volatile uint32_t last_write_open_flags;
        volatile uint32_t last_write_uid;
        volatile uint32_t last_write_gid;
        volatile uint32_t last_write_pid;
    };
    struct mmap_shared_state *shared =
        (struct mmap_shared_state *)mmap(NULL, sizeof(*shared), PROT_READ | PROT_WRITE,
                                         MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (shared == MAP_FAILED) {
        printf("[FAIL] mmap(shared counters): %s (errno=%d)\n", strerror(errno), errno);
        return -1;
    }
    memset(shared, 0, sizeof(*shared));

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }

    daemon = fork();
    if (daemon < 0) {
        printf("[FAIL] fork fuse daemon: %s (errno=%d)\n", strerror(errno), errno);
        close(fd);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (daemon == 0) {
        struct fuse_daemon_args child_args;
        memset(&child_args, 0, sizeof(child_args));
        child_args.fd = fd;
        child_args.stop = &shared->stop;
        child_args.init_done = &shared->init_done;
        child_args.enable_write_ops = 1;
        child_args.stop_on_destroy = 1;
        child_args.open_count = &shared->open_count;
        child_args.read_count = &shared->read_count;
        child_args.write_count = &shared->write_count;
        child_args.last_write_fh = &shared->last_write_fh;
        child_args.last_open_pid = &shared->last_open_pid;
        child_args.last_write_offset = &shared->last_write_offset;
        child_args.last_write_size = &shared->last_write_size;
        child_args.last_write_flags = &shared->last_write_flags;
        child_args.last_write_open_flags = &shared->last_write_open_flags;
        child_args.last_write_uid = &shared->last_write_uid;
        child_args.last_write_gid = &shared->last_write_gid;
        child_args.last_write_pid = &shared->last_write_pid;
        child_args.next_open_fh = 900;
        fuse_daemon_thread(&child_args);
        _exit(0);
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        shared->stop = 1;
        close(fd);
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&shared->init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDWR);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    addr = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, f, 0);
    if (addr == MAP_FAILED) {
        printf("[FAIL] mmap(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        close(f);
        goto fail;
    }

    c = ((volatile char *)addr)[0];
    if (c != 'h') {
        printf("[FAIL] shared writable mmap first byte got=%d\n", c);
        goto fail;
    }
    ((volatile char *)addr)[1] = 'M';
    if (msync(addr, 4096, MS_SYNC) != 0) {
        printf("[FAIL] msync(shared writable mmap): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (shared->open_count != 1 || shared->read_count != 1 || shared->write_count != 1 ||
        shared->last_write_fh != 900 || shared->last_write_offset != 0 ||
        shared->last_write_size != 16 || shared->last_write_flags != expected_writeback_flags ||
        shared->last_write_open_flags != 0 || shared->last_write_uid != 0 ||
        shared->last_write_gid != 0 || shared->last_open_pid == 0 ||
        shared->last_write_pid != shared->last_open_pid) {
        printf("[FAIL] shared writable mmap counters open=%u read=%u write=%u wfh=%llu open_pid=%u off=%llu size=%u wflags=%u oflags=%u uid=%u gid=%u pid=%u\n",
               shared->open_count, shared->read_count, shared->write_count,
               (unsigned long long)shared->last_write_fh, shared->last_open_pid,
               (unsigned long long)shared->last_write_offset, shared->last_write_size,
               shared->last_write_flags, shared->last_write_open_flags, shared->last_write_uid,
               shared->last_write_gid, shared->last_write_pid);
        goto fail;
    }

    munmap(addr, 4096);
    addr = MAP_FAILED;
    close(f);
    f = -1;
    umount(mp);
    shared->stop = 1;
    close(fd);
    waitpid(daemon, NULL, 0);
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return 0;

fail:
    if (addr != MAP_FAILED) {
        munmap(addr, 4096);
    }
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    shared->stop = 1;
    close(fd);
    if (daemon > 0) {
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
    }
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return -1;
}

static int ext_test_shared_mmap_dirty_then_pwrite_keeps_latest_data() {
    const char *mp = "/tmp/test_fuse_mmap_dirty_pwrite";
    char path[256];
    int f = -1;
    void *addr = MAP_FAILED;
    volatile char c = 0;
    pid_t daemon = -1;
    bool saw_direct_pwrite = false;
    bool saw_cache_writeback = false;
    int direct_pwrite_index = -1;
    int stale_cover_index = -1;
    unsigned char stale_cover_byte = 0;
    uint32_t traced_writes = 0;
    struct dirty_pwrite_shared_state {
        volatile int stop;
        volatile int init_done;
        volatile uint32_t read_count;
        volatile uint32_t write_count;
        volatile uint64_t last_write_offset;
        volatile uint32_t last_write_size;
        volatile uint32_t last_write_flags;
        volatile unsigned char last_write_watch_byte;
        volatile uint64_t write_offsets[8];
        volatile uint32_t write_sizes[8];
        volatile uint32_t write_flags[8];
        volatile unsigned char write_watch_bytes[8];
        volatile unsigned char write_covers_watch[8];
        volatile unsigned char backend_watch_byte;
    };
    struct dirty_pwrite_shared_state *shared =
        (struct dirty_pwrite_shared_state *)mmap(NULL, sizeof(*shared), PROT_READ | PROT_WRITE,
                                                 MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (shared == MAP_FAILED) {
        printf("[FAIL] mmap(shared counters): %s (errno=%d)\n", strerror(errno), errno);
        return -1;
    }
    memset(shared, 0, sizeof(*shared));

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }

    daemon = fork();
    if (daemon < 0) {
        printf("[FAIL] fork fuse daemon: %s (errno=%d)\n", strerror(errno), errno);
        close(fd);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (daemon == 0) {
        struct fuse_daemon_args child_args;
        memset(&child_args, 0, sizeof(child_args));
        child_args.fd = fd;
        child_args.stop = &shared->stop;
        child_args.init_done = &shared->init_done;
        child_args.enable_write_ops = 1;
        child_args.stop_on_destroy = 1;
        child_args.read_count = &shared->read_count;
        child_args.write_count = &shared->write_count;
        child_args.last_write_offset = &shared->last_write_offset;
        child_args.last_write_size = &shared->last_write_size;
        child_args.last_write_flags = &shared->last_write_flags;
        child_args.last_write_watch_byte = &shared->last_write_watch_byte;
        child_args.write_offsets = shared->write_offsets;
        child_args.write_sizes = shared->write_sizes;
        child_args.write_flags = shared->write_flags;
        child_args.write_watch_bytes = shared->write_watch_bytes;
        child_args.write_covers_watch = shared->write_covers_watch;
        child_args.backend_watch_byte = &shared->backend_watch_byte;
        child_args.write_trace_capacity = 8;
        child_args.write_watch_offset = 1;
        child_args.next_open_fh = 901;
        fuse_daemon_thread(&child_args);
        _exit(0);
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        shared->stop = 1;
        close(fd);
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&shared->init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDWR);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    addr = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, f, 0);
    if (addr == MAP_FAILED) {
        printf("[FAIL] mmap(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }

    c = ((volatile char *)addr)[0];
    if (c != 'h') {
        printf("[FAIL] shared writable mmap first byte got=%d\n", c);
        goto fail;
    }
    ((volatile char *)addr)[1] = 'M';
    if (pwrite(f, "P", 1, 1) != 1) {
        printf("[FAIL] pwrite over dirty mmap byte: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (((volatile char *)addr)[1] != 'P') {
        printf("[FAIL] mmap cache was not updated by overlapping pwrite got=%d\n",
               ((volatile char *)addr)[1]);
        goto fail;
    }
    if (msync(addr, 4096, MS_SYNC) != 0) {
        printf("[FAIL] msync(shared dirty pwrite): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    __sync_synchronize();
    traced_writes = shared->write_count;
    if (traced_writes > 8) {
        printf("[FAIL] dirty mmap pwrite write trace truncated read=%u write=%u\n",
               shared->read_count, traced_writes);
        goto fail;
    }

    saw_direct_pwrite = false;
    saw_cache_writeback = false;
    direct_pwrite_index = -1;
    stale_cover_index = -1;
    stale_cover_byte = 0;
    for (uint32_t i = 0; i < traced_writes; ++i) {
        uint64_t off = shared->write_offsets[i];
        uint32_t size = shared->write_sizes[i];
        uint32_t flags = shared->write_flags[i];
        unsigned char watch = shared->write_watch_bytes[i];
        bool covers_watch = shared->write_covers_watch[i] != 0;

        if (off == 1 && size == 1 && flags == 0 && watch == 'P') {
            saw_direct_pwrite = true;
            direct_pwrite_index = (int)i;
        }
        if (off == 0 && size == 16 && flags == FUSE_WRITE_CACHE && covers_watch) {
            saw_cache_writeback = true;
        }
        if (direct_pwrite_index >= 0 && (int)i > direct_pwrite_index && covers_watch &&
            watch != 'P') {
            stale_cover_index = (int)i;
            stale_cover_byte = watch;
        }
    }

    if (traced_writes < 2 || !saw_direct_pwrite || !saw_cache_writeback ||
        stale_cover_index >= 0 || shared->backend_watch_byte != 'P') {
        printf("[FAIL] dirty mmap pwrite counters read=%u write=%u last_off=%llu last_size=%u last_flags=%u last_watched=%u backend=%u saw_pwrite=%d saw_cache=%d stale_index=%d stale_byte=%u\n",
               shared->read_count, traced_writes,
               (unsigned long long)shared->last_write_offset, shared->last_write_size,
               shared->last_write_flags, shared->last_write_watch_byte,
               shared->backend_watch_byte, saw_direct_pwrite ? 1 : 0,
               saw_cache_writeback ? 1 : 0, stale_cover_index, stale_cover_byte);
        for (uint32_t i = 0; i < traced_writes; ++i) {
            printf("[FAIL] dirty mmap pwrite write[%u] off=%llu size=%u flags=%u covers=%u watched=%u\n",
                   i, (unsigned long long)shared->write_offsets[i], shared->write_sizes[i],
                   shared->write_flags[i], shared->write_covers_watch[i],
                   shared->write_watch_bytes[i]);
        }
        goto fail;
    }

    munmap(addr, 4096);
    addr = MAP_FAILED;
    close(f);
    f = -1;
    umount(mp);
    shared->stop = 1;
    close(fd);
    waitpid(daemon, NULL, 0);
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return 0;

fail:
    if (addr != MAP_FAILED) {
        munmap(addr, 4096);
    }
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    shared->stop = 1;
    close(fd);
    if (daemon > 0) {
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
    }
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return -1;
}

static int ext_test_shared_writable_mmap_osync_writeback() {
    const char *mp = "/tmp/test_fuse_mmap_shared_osync";
    char path[256];
    int f = -1;
    void *addr = MAP_FAILED;
    volatile char c = 0;
    pid_t daemon = -1;
    const uint32_t expected_writeback_flags = FUSE_WRITE_CACHE;
    const size_t page_size = 4096;
    const size_t map_len = page_size * 2;
    const char marker = 'Z';
    struct mmap_shared_state {
        volatile int stop;
        volatile int init_done;
        volatile uint32_t open_count;
        volatile uint32_t read_count;
        volatile uint32_t write_count;
        volatile uint32_t fsync_count;
        volatile uint64_t last_write_fh;
        volatile uint64_t last_write_offset;
        volatile uint32_t last_write_size;
        volatile uint32_t last_write_flags;
        volatile uint32_t last_write_open_flags;
        volatile uint64_t last_fsync_fh;
        volatile uint32_t write_count_at_fsync;
        volatile uint32_t last_write_flags_at_fsync;
    };
    struct mmap_shared_state *shared =
        (struct mmap_shared_state *)mmap(NULL, sizeof(*shared), PROT_READ | PROT_WRITE,
                                         MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (shared == MAP_FAILED) {
        printf("[FAIL] mmap(shared counters): %s (errno=%d)\n", strerror(errno), errno);
        return -1;
    }
    memset(shared, 0, sizeof(*shared));

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }

    daemon = fork();
    if (daemon < 0) {
        printf("[FAIL] fork fuse daemon: %s (errno=%d)\n", strerror(errno), errno);
        close(fd);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (daemon == 0) {
        struct fuse_daemon_args child_args;
        memset(&child_args, 0, sizeof(child_args));
        child_args.fd = fd;
        child_args.stop = &shared->stop;
        child_args.init_done = &shared->init_done;
        child_args.enable_write_ops = 1;
        child_args.stop_on_destroy = 1;
        child_args.open_count = &shared->open_count;
        child_args.read_count = &shared->read_count;
        child_args.write_count = &shared->write_count;
        child_args.fsync_count = &shared->fsync_count;
        child_args.last_write_fh = &shared->last_write_fh;
        child_args.last_write_offset = &shared->last_write_offset;
        child_args.last_write_size = &shared->last_write_size;
        child_args.last_write_flags = &shared->last_write_flags;
        child_args.last_write_open_flags = &shared->last_write_open_flags;
        child_args.last_fsync_fh = &shared->last_fsync_fh;
        child_args.write_count_at_fsync = &shared->write_count_at_fsync;
        child_args.last_write_flags_at_fsync = &shared->last_write_flags_at_fsync;
        child_args.next_open_fh = 930;
        child_args.hello_data_size_override = map_len;
        fuse_daemon_thread(&child_args);
        _exit(0);
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        shared->stop = 1;
        close(fd);
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&shared->init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDWR | O_SYNC);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    addr = mmap(NULL, map_len, PROT_READ | PROT_WRITE, MAP_SHARED, f, 0);
    if (addr == MAP_FAILED) {
        printf("[FAIL] mmap(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        close(f);
        goto fail;
    }

    c = ((volatile char *)addr)[0];
    if (c != 'A') {
        printf("[FAIL] shared writable mmap first byte got=%d\n", c);
        goto fail;
    }
    ((volatile char *)addr)[2] = 'F';
    if (pwrite(f, &marker, 1, (off_t)page_size) != 1) {
        printf("[FAIL] pwrite(O_SYNC): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    if (shared->open_count != 1 || shared->read_count != 1 || shared->write_count != 2 ||
        shared->fsync_count != 1 || shared->last_write_fh != 930 || shared->last_fsync_fh != 930 ||
        shared->last_write_offset != 0 || shared->last_write_size != page_size ||
        shared->last_write_flags != expected_writeback_flags || shared->last_write_open_flags != 0 ||
        shared->write_count_at_fsync != 2 ||
        shared->last_write_flags_at_fsync != expected_writeback_flags) {
        printf("[FAIL] shared mmap osync counters open=%u read=%u write=%u fsync=%u wfh=%llu fsh=%llu off=%llu size=%u wflags=%u oflags=%u fsync_writes=%u fsync_wflags=%u\n",
               shared->open_count, shared->read_count, shared->write_count, shared->fsync_count,
               (unsigned long long)shared->last_write_fh,
               (unsigned long long)shared->last_fsync_fh,
               (unsigned long long)shared->last_write_offset, shared->last_write_size,
               shared->last_write_flags, shared->last_write_open_flags,
               shared->write_count_at_fsync, shared->last_write_flags_at_fsync);
        goto fail;
    }

    munmap(addr, map_len);
    addr = MAP_FAILED;
    close(f);
    f = -1;
    umount(mp);
    shared->stop = 1;
    close(fd);
    waitpid(daemon, NULL, 0);
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return 0;

fail:
    if (addr != MAP_FAILED) {
        munmap(addr, map_len);
    }
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    shared->stop = 1;
    close(fd);
    if (daemon > 0) {
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
    }
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return -1;
}

static int ext_test_shared_mmap_mprotect_writeback() {
    const char *mp = "/tmp/test_fuse_mmap_mprotect_write";
    char path[256];
    int f = -1;
    void *addr = MAP_FAILED;
    volatile char c = 0;
    pid_t daemon = -1;
    struct mmap_shared_state {
        volatile int stop;
        volatile int init_done;
        volatile uint32_t open_count;
        volatile uint32_t read_count;
        volatile uint32_t write_count;
        volatile uint64_t last_write_fh;
    };
    struct mmap_shared_state *shared =
        (struct mmap_shared_state *)mmap(NULL, sizeof(*shared), PROT_READ | PROT_WRITE,
                                         MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (shared == MAP_FAILED) {
        printf("[FAIL] mmap(shared counters): %s (errno=%d)\n", strerror(errno), errno);
        return -1;
    }
    memset(shared, 0, sizeof(*shared));

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }

    daemon = fork();
    if (daemon < 0) {
        printf("[FAIL] fork fuse daemon: %s (errno=%d)\n", strerror(errno), errno);
        close(fd);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (daemon == 0) {
        struct fuse_daemon_args child_args;
        memset(&child_args, 0, sizeof(child_args));
        child_args.fd = fd;
        child_args.stop = &shared->stop;
        child_args.init_done = &shared->init_done;
        child_args.enable_write_ops = 1;
        child_args.stop_on_destroy = 1;
        child_args.open_count = &shared->open_count;
        child_args.read_count = &shared->read_count;
        child_args.write_count = &shared->write_count;
        child_args.last_write_fh = &shared->last_write_fh;
        child_args.next_open_fh = 910;
        fuse_daemon_thread(&child_args);
        _exit(0);
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        shared->stop = 1;
        close(fd);
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&shared->init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDWR);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    addr = mmap(NULL, 4096, PROT_READ, MAP_SHARED, f, 0);
    if (addr == MAP_FAILED) {
        printf("[FAIL] mmap(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        close(f);
        goto fail;
    }

    c = ((volatile char *)addr)[0];
    if (c != 'h') {
        printf("[FAIL] mmap first byte got=%d\n", c);
        goto fail;
    }
    if (shared->open_count != 1 || shared->read_count != 1) {
        printf("[FAIL] before mprotect counters open=%u read=%u\n", shared->open_count,
               shared->read_count);
        goto fail;
    }
    if (mprotect(addr, 4096, PROT_READ | PROT_WRITE) != 0) {
        printf("[FAIL] mprotect shared writable FUSE mapping: %s (errno=%d)\n", strerror(errno),
               errno);
        goto fail;
    }
    ((volatile char *)addr)[2] = 'P';
    if (msync(addr, 4096, MS_SYNC) != 0) {
        printf("[FAIL] msync(after mprotect): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (shared->open_count != 1 || shared->read_count != 1 || shared->write_count != 1 ||
        shared->last_write_fh != 910) {
        printf("[FAIL] after mprotect counters open=%u read=%u write=%u wfh=%llu\n",
               shared->open_count, shared->read_count, shared->write_count,
               (unsigned long long)shared->last_write_fh);
        goto fail;
    }

    munmap(addr, 4096);
    addr = MAP_FAILED;
    close(f);
    f = -1;
    umount(mp);
    shared->stop = 1;
    close(fd);
    waitpid(daemon, NULL, 0);
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return 0;

fail:
    if (addr != MAP_FAILED) {
        munmap(addr, 4096);
    }
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    shared->stop = 1;
    close(fd);
    if (daemon > 0) {
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
    }
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return -1;
}

static int ext_test_shared_mmap_readonly_fd_mprotect_write_denied() {
    const char *mp = "/tmp/test_fuse_mmap_readonly_mprotect";
    char path[256];
    int f = -1;
    void *addr = MAP_FAILED;
    volatile char c = 0;
    pid_t daemon = -1;
    struct mmap_shared_state {
        volatile int stop;
        volatile int init_done;
        volatile uint32_t open_count;
        volatile uint32_t read_count;
        volatile uint32_t write_count;
        volatile uint64_t last_write_fh;
    };
    struct mmap_shared_state *shared =
        (struct mmap_shared_state *)mmap(NULL, sizeof(*shared), PROT_READ | PROT_WRITE,
                                         MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (shared == MAP_FAILED) {
        printf("[FAIL] mmap(shared counters): %s (errno=%d)\n", strerror(errno), errno);
        return -1;
    }
    memset(shared, 0, sizeof(*shared));

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }

    daemon = fork();
    if (daemon < 0) {
        printf("[FAIL] fork fuse daemon: %s (errno=%d)\n", strerror(errno), errno);
        close(fd);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (daemon == 0) {
        struct fuse_daemon_args child_args;
        memset(&child_args, 0, sizeof(child_args));
        child_args.fd = fd;
        child_args.stop = &shared->stop;
        child_args.init_done = &shared->init_done;
        child_args.enable_write_ops = 1;
        child_args.stop_on_destroy = 1;
        child_args.open_count = &shared->open_count;
        child_args.read_count = &shared->read_count;
        child_args.write_count = &shared->write_count;
        child_args.last_write_fh = &shared->last_write_fh;
        child_args.next_open_fh = 930;
        fuse_daemon_thread(&child_args);
        _exit(0);
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        shared->stop = 1;
        close(fd);
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&shared->init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDONLY);
    if (f < 0) {
        printf("[FAIL] open(%s, O_RDONLY): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    addr = mmap(NULL, 4096, PROT_READ, MAP_SHARED, f, 0);
    if (addr == MAP_FAILED) {
        printf("[FAIL] mmap(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        close(f);
        goto fail;
    }

    c = ((volatile char *)addr)[0];
    if (c != 'h') {
        printf("[FAIL] readonly shared mmap first byte got=%d\n", c);
        goto fail;
    }
    errno = 0;
    if (mprotect(addr, 4096, PROT_READ | PROT_WRITE) == 0) {
        printf("[FAIL] mprotect unexpectedly allowed write upgrade on readonly fd\n");
        goto fail;
    }
    if (shared->open_count != 1 || shared->read_count != 1 || shared->write_count != 0 ||
        shared->last_write_fh != 0) {
        printf("[FAIL] readonly mprotect counters open=%u read=%u write=%u wfh=%llu\n",
               shared->open_count, shared->read_count, shared->write_count,
               (unsigned long long)shared->last_write_fh);
        goto fail;
    }

    munmap(addr, 4096);
    addr = MAP_FAILED;
    close(f);
    f = -1;
    umount(mp);
    shared->stop = 1;
    close(fd);
    waitpid(daemon, NULL, 0);
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return 0;

fail:
    if (addr != MAP_FAILED) {
        munmap(addr, 4096);
    }
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    shared->stop = 1;
    close(fd);
    if (daemon > 0) {
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
    }
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return -1;
}

static int ext_test_shared_writable_mmap_munmap_writeback_without_msync() {
    const char *mp = "/tmp/test_fuse_mmap_munmap_writeback";
    char path[256];
    int f = -1;
    void *addr = MAP_FAILED;
    const uint32_t expected_writeback_flags = FUSE_WRITE_CACHE;
    pid_t daemon = -1;
    struct mmap_shared_state {
        volatile int stop;
        volatile int init_done;
        volatile uint32_t open_count;
        volatile uint32_t read_count;
        volatile uint32_t write_count;
        volatile uint64_t last_write_fh;
        volatile uint64_t last_write_offset;
        volatile uint32_t last_write_size;
        volatile uint32_t last_write_flags;
        volatile uint32_t last_write_open_flags;
    };
    struct mmap_shared_state *shared =
        (struct mmap_shared_state *)mmap(NULL, sizeof(*shared), PROT_READ | PROT_WRITE,
                                         MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (shared == MAP_FAILED) {
        printf("[FAIL] mmap(shared counters): %s (errno=%d)\n", strerror(errno), errno);
        return -1;
    }
    memset(shared, 0, sizeof(*shared));

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }

    daemon = fork();
    if (daemon < 0) {
        printf("[FAIL] fork fuse daemon: %s (errno=%d)\n", strerror(errno), errno);
        close(fd);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (daemon == 0) {
        struct fuse_daemon_args child_args;
        memset(&child_args, 0, sizeof(child_args));
        child_args.fd = fd;
        child_args.stop = &shared->stop;
        child_args.init_done = &shared->init_done;
        child_args.enable_write_ops = 1;
        child_args.stop_on_destroy = 1;
        child_args.open_count = &shared->open_count;
        child_args.read_count = &shared->read_count;
        child_args.write_count = &shared->write_count;
        child_args.last_write_fh = &shared->last_write_fh;
        child_args.last_write_offset = &shared->last_write_offset;
        child_args.last_write_size = &shared->last_write_size;
        child_args.last_write_flags = &shared->last_write_flags;
        child_args.last_write_open_flags = &shared->last_write_open_flags;
        child_args.next_open_fh = 940;
        fuse_daemon_thread(&child_args);
        _exit(0);
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        shared->stop = 1;
        close(fd);
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&shared->init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDWR);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    addr = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, f, 0);
    if (addr == MAP_FAILED) {
        printf("[FAIL] mmap(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        close(f);
        goto fail;
    }

    if (((volatile char *)addr)[0] != 'h') {
        printf("[FAIL] shared close-writeback mmap first byte got=%d\n",
               ((volatile char *)addr)[0]);
        goto fail;
    }
    ((volatile char *)addr)[3] = 'C';
    if (munmap(addr, 4096) != 0) {
        printf("[FAIL] munmap(shared writable mmap): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    addr = MAP_FAILED;

    if (shared->open_count != 1 || shared->read_count != 1 || shared->write_count != 1 ||
        shared->last_write_fh != 940 || shared->last_write_offset != 0 ||
        shared->last_write_size != 16 || shared->last_write_flags != expected_writeback_flags ||
        shared->last_write_open_flags != 0) {
        printf("[FAIL] munmap writeback counters open=%u read=%u write=%u wfh=%llu off=%llu size=%u wflags=%u oflags=%u\n",
               shared->open_count, shared->read_count, shared->write_count,
               (unsigned long long)shared->last_write_fh,
               (unsigned long long)shared->last_write_offset, shared->last_write_size,
               shared->last_write_flags, shared->last_write_open_flags);
        goto fail;
    }

    close(f);
    f = -1;
    umount(mp);
    shared->stop = 1;
    close(fd);
    waitpid(daemon, NULL, 0);
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return 0;

fail:
    if (addr != MAP_FAILED) {
        munmap(addr, 4096);
    }
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    shared->stop = 1;
    close(fd);
    if (daemon > 0) {
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
    }
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return -1;
}

static int ext_test_shared_mmap_subrange_mprotect_writeback_preserves_vma() {
    const char *mp = "/tmp/test_fuse_mmap_mprotect_subrange";
    const size_t page_size = 4096;
    const size_t map_len = page_size * 2;
    char path[256];
    int f = -1;
    void *addr = MAP_FAILED;
    volatile char c = 0;
    pid_t daemon = -1;
    struct sigaction old_segv;
    bool segv_handler_installed = false;
    struct mmap_shared_state {
        volatile int stop;
        volatile int init_done;
        volatile uint32_t open_count;
        volatile uint32_t read_count;
        volatile uint32_t write_count;
        volatile uint64_t last_write_fh;
    };
    struct mmap_shared_state *shared =
        (struct mmap_shared_state *)mmap(NULL, sizeof(*shared), PROT_READ | PROT_WRITE,
                                         MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (shared == MAP_FAILED) {
        printf("[FAIL] mmap(shared counters): %s (errno=%d)\n", strerror(errno), errno);
        return -1;
    }
    memset(shared, 0, sizeof(*shared));

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }

    daemon = fork();
    if (daemon < 0) {
        printf("[FAIL] fork fuse daemon: %s (errno=%d)\n", strerror(errno), errno);
        close(fd);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (daemon == 0) {
        struct fuse_daemon_args child_args;
        memset(&child_args, 0, sizeof(child_args));
        child_args.fd = fd;
        child_args.stop = &shared->stop;
        child_args.init_done = &shared->init_done;
        child_args.stop_on_destroy = 1;
        child_args.enable_write_ops = 1;
        child_args.open_count = &shared->open_count;
        child_args.read_count = &shared->read_count;
        child_args.write_count = &shared->write_count;
        child_args.last_write_fh = &shared->last_write_fh;
        child_args.hello_data_size_override = map_len;
        child_args.next_open_fh = 920;
        fuse_daemon_thread(&child_args);
        _exit(0);
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        shared->stop = 1;
        close(fd);
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&shared->init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDWR);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    addr = mmap(NULL, map_len, PROT_READ, MAP_SHARED, f, 0);
    if (addr == MAP_FAILED) {
        printf("[FAIL] mmap(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        close(f);
        goto fail;
    }

    c = ((volatile char *)addr)[0];
    if (c != 'A') {
        printf("[FAIL] first page byte got=%d\n", c);
        goto fail;
    }
    c = ((volatile char *)addr)[page_size];
    if (c != 'O') {
        printf("[FAIL] second page byte got=%d\n", c);
        goto fail;
    }
    if (shared->open_count != 1 || shared->read_count != 2) {
        printf("[FAIL] before subrange mprotect counters open=%u read=%u\n",
               shared->open_count, shared->read_count);
        goto fail;
    }
    if (mprotect((char *)addr + page_size, page_size, PROT_READ | PROT_WRITE) != 0) {
        printf("[FAIL] subrange mprotect(shared writable): %s (errno=%d)\n", strerror(errno),
               errno);
        goto fail;
    }
    ((volatile char *)addr)[page_size + 1] = 'S';
    if (msync((char *)addr + page_size, page_size, MS_SYNC) != 0) {
        printf("[FAIL] msync(subrange shared writable): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (shared->write_count != 1 || shared->last_write_fh != 920) {
        printf("[FAIL] subrange writeback counters write=%u wfh=%llu\n", shared->write_count,
               (unsigned long long)shared->last_write_fh);
        goto fail;
    }
    if (mprotect(addr, page_size, PROT_NONE) != 0) {
        printf("[FAIL] mprotect(PROT_NONE first page): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_handler = fuse_sigsegv_longjmp_handler;
    sigemptyset(&sa.sa_mask);
    if (sigaction(SIGSEGV, &sa, &old_segv) != 0) {
        printf("[FAIL] sigaction(SIGSEGV): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    segv_handler_installed = true;
    g_fuse_sigsegv_seen = 0;
    if (sigsetjmp(g_fuse_sigsegv_jmp, 1) == 0) {
        c = ((volatile char *)addr)[0];
        (void)c;
    }
    sigaction(SIGSEGV, &old_segv, NULL);
    segv_handler_installed = false;
    if (!g_fuse_sigsegv_seen) {
        printf("[FAIL] first page remained readable after PROT_NONE\n");
        goto fail;
    }

    munmap(addr, map_len);
    addr = MAP_FAILED;
    close(f);
    f = -1;
    umount(mp);
    shared->stop = 1;
    close(fd);
    waitpid(daemon, NULL, 0);
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return 0;

fail:
    if (segv_handler_installed) {
        sigaction(SIGSEGV, &old_segv, NULL);
    }
    if (addr != MAP_FAILED) {
        munmap(addr, map_len);
    }
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    shared->stop = 1;
    close(fd);
    if (daemon > 0) {
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
    }
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return -1;
}

static int ext_test_shared_mmap_unfaulted_mprotect_prot_none() {
    const char *mp = "/tmp/test_fuse_mmap_unfaulted_mprotect";
    const size_t page_size = 4096;
    char path[256];
    int f = -1;
    void *addr = MAP_FAILED;
    volatile char c = 0;
    pid_t daemon = -1;
    struct sigaction old_segv;
    bool segv_handler_installed = false;
    struct mmap_shared_state {
        volatile int stop;
        volatile int init_done;
        volatile uint32_t open_count;
        volatile uint32_t read_count;
    };
    struct mmap_shared_state *shared =
        (struct mmap_shared_state *)mmap(NULL, sizeof(*shared), PROT_READ | PROT_WRITE,
                                         MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (shared == MAP_FAILED) {
        printf("[FAIL] mmap(shared counters): %s (errno=%d)\n", strerror(errno), errno);
        return -1;
    }
    memset(shared, 0, sizeof(*shared));

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }

    daemon = fork();
    if (daemon < 0) {
        printf("[FAIL] fork fuse daemon: %s (errno=%d)\n", strerror(errno), errno);
        close(fd);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (daemon == 0) {
        struct fuse_daemon_args child_args;
        memset(&child_args, 0, sizeof(child_args));
        child_args.fd = fd;
        child_args.stop = &shared->stop;
        child_args.init_done = &shared->init_done;
        child_args.stop_on_destroy = 1;
        child_args.open_count = &shared->open_count;
        child_args.read_count = &shared->read_count;
        child_args.hello_data_size_override = page_size;
        fuse_daemon_thread(&child_args);
        _exit(0);
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        shared->stop = 1;
        close(fd);
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&shared->init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDWR);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    addr = mmap(NULL, page_size, PROT_READ, MAP_SHARED, f, 0);
    if (addr == MAP_FAILED) {
        printf("[FAIL] mmap(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        close(f);
        goto fail;
    }
    if (shared->open_count != 1 || shared->read_count != 0) {
        printf("[FAIL] before unfaulted mprotect counters open=%u read=%u\n",
               shared->open_count, shared->read_count);
        goto fail;
    }
    if (mprotect(addr, page_size, PROT_NONE) != 0) {
        printf("[FAIL] mprotect(PROT_NONE unfaulted): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_handler = fuse_sigsegv_longjmp_handler;
    sigemptyset(&sa.sa_mask);
    if (sigaction(SIGSEGV, &sa, &old_segv) != 0) {
        printf("[FAIL] sigaction(SIGSEGV): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    segv_handler_installed = true;
    g_fuse_sigsegv_seen = 0;
    if (sigsetjmp(g_fuse_sigsegv_jmp, 1) == 0) {
        c = ((volatile char *)addr)[0];
        (void)c;
    }
    sigaction(SIGSEGV, &old_segv, NULL);
    segv_handler_installed = false;
    if (!g_fuse_sigsegv_seen) {
        printf("[FAIL] unfaulted PROT_NONE mapping remained readable\n");
        goto fail;
    }
    if (shared->read_count != 0) {
        printf("[FAIL] unfaulted PROT_NONE triggered read_count=%u\n", shared->read_count);
        goto fail;
    }

    munmap(addr, page_size);
    addr = MAP_FAILED;
    close(f);
    f = -1;
    umount(mp);
    shared->stop = 1;
    close(fd);
    waitpid(daemon, NULL, 0);
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return 0;

fail:
    if (segv_handler_installed) {
        sigaction(SIGSEGV, &old_segv, NULL);
    }
    if (addr != MAP_FAILED) {
        munmap(addr, page_size);
    }
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    shared->stop = 1;
    close(fd);
    if (daemon > 0) {
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
    }
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return -1;
}

static int ext_test_mmap_truncate_unmaps_stale_page() {
    const char *mp = "/tmp/test_fuse_mmap_truncate";
    const size_t page_size = 4096;
    const size_t map_len = page_size * 2;
    char path[256];
    int f = -1;
    void *addr = MAP_FAILED;
    volatile char c = 0;
    pid_t daemon = -1;
    struct sigaction old_bus;
    bool bus_handler_installed = false;
    struct mmap_shared_state {
        volatile int stop;
        volatile int init_done;
        volatile uint32_t open_count;
        volatile uint32_t read_count;
    };
    struct mmap_shared_state *shared =
        (struct mmap_shared_state *)mmap(NULL, sizeof(*shared), PROT_READ | PROT_WRITE,
                                         MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (shared == MAP_FAILED) {
        printf("[FAIL] mmap(shared counters): %s (errno=%d)\n", strerror(errno), errno);
        return -1;
    }
    memset(shared, 0, sizeof(*shared));

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }

    daemon = fork();
    if (daemon < 0) {
        printf("[FAIL] fork fuse daemon: %s (errno=%d)\n", strerror(errno), errno);
        close(fd);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (daemon == 0) {
        struct fuse_daemon_args child_args;
        memset(&child_args, 0, sizeof(child_args));
        child_args.fd = fd;
        child_args.stop = &shared->stop;
        child_args.init_done = &shared->init_done;
        child_args.stop_on_destroy = 1;
        child_args.enable_write_ops = 1;
        child_args.open_count = &shared->open_count;
        child_args.read_count = &shared->read_count;
        child_args.hello_data_size_override = map_len;
        fuse_daemon_thread(&child_args);
        _exit(0);
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        shared->stop = 1;
        close(fd);
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&shared->init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDWR);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    addr = mmap(NULL, map_len, PROT_READ, MAP_PRIVATE, f, 0);
    if (addr == MAP_FAILED) {
        printf("[FAIL] mmap(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        close(f);
        goto fail;
    }

    c = ((volatile char *)addr)[page_size];
    if (c != 'O') {
        printf("[FAIL] second page byte before truncate got=%d\n", c);
        goto fail;
    }
    if (shared->open_count != 1 || shared->read_count != 1) {
        printf("[FAIL] before truncate counters open=%u read=%u\n", shared->open_count,
               shared->read_count);
        goto fail;
    }
    if (ftruncate(f, page_size) != 0) {
        printf("[FAIL] ftruncate: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_handler = fuse_sigbus_longjmp_handler;
    sigemptyset(&sa.sa_mask);
    if (sigaction(SIGBUS, &sa, &old_bus) != 0) {
        printf("[FAIL] sigaction(SIGBUS): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    bus_handler_installed = true;
    g_fuse_sigbus_seen = 0;
    if (sigsetjmp(g_fuse_sigbus_jmp, 1) == 0) {
        c = ((volatile char *)addr)[page_size];
        (void)c;
    }
    sigaction(SIGBUS, &old_bus, NULL);
    bus_handler_installed = false;
    if (!g_fuse_sigbus_seen) {
        printf("[FAIL] truncated second page remained readable read=%u\n", shared->read_count);
        goto fail;
    }
    if (shared->read_count != 1) {
        printf("[FAIL] truncated EOF fault issued extra FUSE_READ count=%u\n", shared->read_count);
        goto fail;
    }

    munmap(addr, map_len);
    addr = MAP_FAILED;
    close(f);
    f = -1;
    umount(mp);
    shared->stop = 1;
    close(fd);
    waitpid(daemon, NULL, 0);
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return 0;

fail:
    if (bus_handler_installed) {
        sigaction(SIGBUS, &old_bus, NULL);
    }
    if (addr != MAP_FAILED) {
        munmap(addr, map_len);
    }
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    shared->stop = 1;
    close(fd);
    if (daemon > 0) {
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
    }
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return -1;
}

static int ext_test_fadvise_without_page_cache() {
    const char *mp = "/tmp/test_fuse_fadvise";
    char path[256];
    int f = -1;
    const int advices[] = {
        POSIX_FADV_NORMAL,     POSIX_FADV_RANDOM, POSIX_FADV_SEQUENTIAL,
        POSIX_FADV_WILLNEED,   POSIX_FADV_DONTNEED,
        POSIX_FADV_NOREUSE,
    };

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.stop_on_destroy = 1;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDONLY);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }

    for (size_t i = 0; i < sizeof(advices) / sizeof(advices[0]); i++) {
        int rc = posix_fadvise(f, 0, 0, advices[i]);
        if (rc != 0) {
            printf("[FAIL] posix_fadvise(advice=%d): rc=%d\n", advices[i], rc);
            goto fail;
        }
    }

    if (posix_fadvise(f, 0, -1, POSIX_FADV_NORMAL) != EINVAL) {
        printf("[FAIL] posix_fadvise negative len should return EINVAL\n");
        goto fail;
    }

    close(f);
    f = -1;
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_mount_on_fuse_dir_uses_namespace_path() {
    const char *mp = "/tmp/test_fuse_mount_target";
    char dir_path[512];
    char marker_path[1024];
    int ramfs_mounted = 0;

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(dir_path, sizeof(dir_path), "%s/ramfs_target", mp);
    if (mkdir(dir_path, 0755) != 0) {
        printf("[FAIL] mkdir(%s): %s (errno=%d)\n", dir_path, strerror(errno), errno);
        goto fail;
    }

    if (mount("", dir_path, "ramfs", 0, NULL) != 0) {
        printf("[FAIL] mount(ramfs on fuse dir): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    ramfs_mounted = 1;

    snprintf(marker_path, sizeof(marker_path), "%s/marker", dir_path);
    if (fuseg_write_file(marker_path, "mounted") != 0) {
        printf("[FAIL] write marker under ramfs: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    if (umount(dir_path) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", dir_path, strerror(errno), errno);
        goto fail_no_ramfs_umount;
    }
    ramfs_mounted = 0;
    if (rmdir(dir_path) != 0) {
        printf("[FAIL] rmdir(%s): %s (errno=%d)\n", dir_path, strerror(errno), errno);
        goto fail;
    }

    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (ramfs_mounted) {
        umount(dir_path);
    }
fail_no_ramfs_umount:
    rmdir(dir_path);
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_rename_updates_fuse_dir_cwd_path() {
    const char *mp = "/tmp/test_fuse_rename_path";
    char old_path[512];
    char new_path[512];
    char cwd[512];
    int dir_fd = -1;
    int ramfs_mounted = 0;

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(old_path, sizeof(old_path), "%s/old_dir", mp);
    snprintf(new_path, sizeof(new_path), "%s/new_dir", mp);
    if (mkdir(old_path, 0755) != 0) {
        printf("[FAIL] mkdir(%s): %s (errno=%d)\n", old_path, strerror(errno), errno);
        goto fail;
    }
    dir_fd = open(old_path, O_RDONLY | O_DIRECTORY);
    if (dir_fd < 0) {
        printf("[FAIL] open dir fd %s: %s (errno=%d)\n", old_path, strerror(errno), errno);
        goto fail;
    }
    if (rename(old_path, new_path) != 0) {
        printf("[FAIL] rename(%s -> %s): %s (errno=%d)\n", old_path, new_path, strerror(errno),
               errno);
        goto fail;
    }
    if (fchdir(dir_fd) != 0) {
        printf("[FAIL] fchdir renamed dir fd: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (!getcwd(cwd, sizeof(cwd))) {
        printf("[FAIL] getcwd after rename: %s (errno=%d)\n", strerror(errno), errno);
        goto fail_chdir_root;
    }
    if (strcmp(cwd, new_path) != 0) {
        printf("[FAIL] getcwd after rename: got '%s', want '%s'\n", cwd, new_path);
        goto fail_chdir_root;
    }
    if (chdir("/") != 0) {
        printf("[FAIL] chdir(/): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    close(dir_fd);
    dir_fd = -1;

    if (mount("", new_path, "ramfs", 0, NULL) != 0) {
        printf("[FAIL] mount(ramfs on renamed fuse dir): %s (errno=%d)\n", strerror(errno),
               errno);
        goto fail;
    }
    ramfs_mounted = 1;
    if (umount(new_path) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", new_path, strerror(errno), errno);
        goto fail_no_ramfs_umount;
    }
    ramfs_mounted = 0;
    if (rmdir(new_path) != 0) {
        printf("[FAIL] rmdir(%s): %s (errno=%d)\n", new_path, strerror(errno), errno);
        goto fail;
    }

    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail_chdir_root:
    {
        int ignored_chdir = chdir("/");
        (void)ignored_chdir;
    }
fail:
    if (dir_fd >= 0) {
        close(dir_fd);
    }
    if (ramfs_mounted) {
        umount(new_path);
    }
fail_no_ramfs_umount:
    rmdir(new_path);
    rmdir(old_path);
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_lookup_nodes_forgotten_before_umount_when_unreferenced() {
    const char *mp = "/tmp/test_fuse_lookup_lifetime";
    char parent_path[512];
    char child_path[512];
    struct stat st;

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t forget_count = 0;
    volatile uint64_t forget_nlookup_sum = 0;
    volatile uint64_t forget_trace_nodeids[32] = {0};
    volatile uint64_t forget_trace_nlookups[32] = {0};
    volatile uint32_t destroy_count = 0;
    uint32_t forget_count_before_umount = 0;
    uint64_t forget_sum_before_umount = 0;
    uint32_t distinct_nonroot_before_umount = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;
    args.forget_count = &forget_count;
    args.forget_nlookup_sum = &forget_nlookup_sum;
    args.forget_trace_nodeids = forget_trace_nodeids;
    args.forget_trace_nlookups = forget_trace_nlookups;
    args.forget_trace_capacity = 32;
    args.destroy_count = &destroy_count;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(parent_path, sizeof(parent_path), "%s/parent", mp);
    snprintf(child_path, sizeof(child_path), "%s/parent/child", mp);
    if (mkdir(parent_path, 0755) != 0) {
        printf("[FAIL] mkdir(%s): %s (errno=%d)\n", parent_path, strerror(errno), errno);
        goto fail;
    }
    if (mkdir(child_path, 0755) != 0) {
        printf("[FAIL] mkdir(%s): %s (errno=%d)\n", child_path, strerror(errno), errno);
        goto fail;
    }
    if (stat(child_path, &st) != 0 || !S_ISDIR(st.st_mode)) {
        printf("[FAIL] stat(%s): %s (errno=%d)\n", child_path, strerror(errno), errno);
        goto fail;
    }
    if (stat(parent_path, &st) != 0 || !S_ISDIR(st.st_mode)) {
        printf("[FAIL] stat(%s) after child lookup: %s (errno=%d)\n", parent_path,
               strerror(errno), errno);
        goto fail;
    }

    for (int i = 0; i < 200 && forget_nlookup_sum < 2; i++) {
        usleep(10 * 1000);
    }
    if (forget_count == 0 || forget_nlookup_sum < 2) {
        printf("[FAIL] unreferenced FUSE lookup nodes not forgotten before umount: "
               "count=%u nlookup=%llu\n",
               forget_count, (unsigned long long)forget_nlookup_sum);
        goto fail;
    }
    for (uint32_t i = 0; i < forget_count && i < 32; i++) {
        if (forget_trace_nodeids[i] == 1) {
            printf("[FAIL] root node unexpectedly forgotten before umount at index=%u "
                   "nlookup=%llu\n",
                   i, (unsigned long long)forget_trace_nlookups[i]);
            goto fail;
        }
    }
    distinct_nonroot_before_umount = 0;
    for (uint32_t i = 0; i < forget_count && i < 32; i++) {
        if (forget_trace_nodeids[i] == 0 || forget_trace_nodeids[i] == 1) {
            continue;
        }
        bool seen = false;
        for (uint32_t j = 0; j < i; j++) {
            if (forget_trace_nodeids[j] == forget_trace_nodeids[i]) {
                seen = true;
                break;
            }
        }
        if (!seen) {
            distinct_nonroot_before_umount++;
        }
    }
    if (distinct_nonroot_before_umount < 2) {
        printf("[FAIL] expected at least two distinct non-root nodes forgotten before umount, "
               "got=%u count=%u nlookup=%llu\n",
               distinct_nonroot_before_umount, forget_count,
               (unsigned long long)forget_nlookup_sum);
        goto fail;
    }

    forget_count_before_umount = forget_count;
    forget_sum_before_umount = forget_nlookup_sum;

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail;
    }
    for (int i = 0; i < 200 && destroy_count == 0; i++) {
        usleep(10 * 1000);
    }
    if (destroy_count == 0) {
        printf("[FAIL] timed out waiting for FUSE_DESTROY after umount\n");
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    close(fd);
    pthread_join(th, NULL);
    if (destroy_count != 1 || forget_count < forget_count_before_umount ||
        forget_nlookup_sum < forget_sum_before_umount) {
        printf("[FAIL] FUSE teardown lost forget accounting or missed destroy: "
               "forget=%u/%u nlookup=%llu/%llu destroy=%u\n",
               forget_count, forget_count_before_umount, (unsigned long long)forget_nlookup_sum,
               (unsigned long long)forget_sum_before_umount, destroy_count);
        rmdir(mp);
        return -1;
    }
    for (uint32_t i = 0; i < forget_count && i < 32; i++) {
        if (forget_trace_nodeids[i] == 1) {
            printf("[FAIL] root node unexpectedly forgotten at index=%u nlookup=%llu\n", i,
                   (unsigned long long)forget_trace_nlookups[i]);
            rmdir(mp);
            return -1;
        }
    }
    rmdir(mp);
    return 0;

fail:
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static bool forget_trace_contains(volatile uint64_t *nodeids, uint32_t count, uint64_t nodeid) {
    for (uint32_t i = 0; i < count && i < 32; i++) {
        if (nodeids[i] == nodeid) {
            return true;
        }
    }
    return false;
}

static int ext_test_positive_lookup_cache_expires_and_forgets_before_umount() {
    const char *mp = "/tmp/test_fuse_positive_lookup_lifetime";
    char parent_path[512];
    char child_path[512];
    char hello_path[512];
    struct stat st;

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t forget_count = 0;
    volatile uint64_t forget_nlookup_sum = 0;
    volatile uint64_t forget_trace_nodeids[32] = {0};
    volatile uint64_t forget_trace_nlookups[32] = {0};
    volatile uint32_t destroy_count = 0;
    uint32_t forget_count_before_umount = 0;
    uint64_t forget_sum_before_umount = 0;
    uint64_t parent_nodeid = 0;
    uint64_t child_nodeid = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;
    args.forget_count = &forget_count;
    args.forget_nlookup_sum = &forget_nlookup_sum;
    args.forget_trace_nodeids = forget_trace_nodeids;
    args.forget_trace_nlookups = forget_trace_nlookups;
    args.forget_trace_capacity = 32;
    args.destroy_count = &destroy_count;
    args.entry_valid_sec = 1;
    args.attr_valid_sec = 1;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(parent_path, sizeof(parent_path), "%s/parent", mp);
    snprintf(child_path, sizeof(child_path), "%s/parent/child", mp);
    snprintf(hello_path, sizeof(hello_path), "%s/hello.txt", mp);
    if (mkdir(parent_path, 0755) != 0) {
        printf("[FAIL] mkdir(%s): %s (errno=%d)\n", parent_path, strerror(errno), errno);
        goto fail;
    }
    if (stat(parent_path, &st) != 0 || !S_ISDIR(st.st_mode)) {
        printf("[FAIL] stat(%s): %s (errno=%d)\n", parent_path, strerror(errno), errno);
        goto fail;
    }
    parent_nodeid = (uint64_t)st.st_ino;
    if (mkdir(child_path, 0755) != 0) {
        printf("[FAIL] mkdir(%s): %s (errno=%d)\n", child_path, strerror(errno), errno);
        goto fail;
    }
    if (stat(child_path, &st) != 0 || !S_ISDIR(st.st_mode)) {
        printf("[FAIL] stat(%s): %s (errno=%d)\n", child_path, strerror(errno), errno);
        goto fail;
    }
    child_nodeid = (uint64_t)st.st_ino;

    usleep(2000 * 1000);
    if (stat(hello_path, &st) != 0 || !S_ISREG(st.st_mode)) {
        printf("[FAIL] stat(%s) after TTL: %s (errno=%d)\n", hello_path, strerror(errno), errno);
        goto fail;
    }

    for (int i = 0; i < 200; i++) {
        uint32_t count = forget_count;
        if (forget_trace_contains(forget_trace_nodeids, count, parent_nodeid) &&
            forget_trace_contains(forget_trace_nodeids, count, child_nodeid)) {
            break;
        }
        usleep(10 * 1000);
    }
    if (!forget_trace_contains(forget_trace_nodeids, forget_count, parent_nodeid) ||
        !forget_trace_contains(forget_trace_nodeids, forget_count, child_nodeid)) {
        printf("[FAIL] positive TTL cache-only nodes were not forgotten before umount: "
               "count=%u nlookup=%llu parent=%llu child=%llu saw_parent=%d saw_child=%d\n",
               forget_count, (unsigned long long)forget_nlookup_sum,
               (unsigned long long)parent_nodeid, (unsigned long long)child_nodeid,
               forget_trace_contains(forget_trace_nodeids, forget_count, parent_nodeid),
               forget_trace_contains(forget_trace_nodeids, forget_count, child_nodeid));
        goto fail;
    }
    if (forget_trace_contains(forget_trace_nodeids, forget_count, 1)) {
        printf("[FAIL] root node unexpectedly forgotten before umount\n");
        goto fail;
    }

    forget_count_before_umount = forget_count;
    forget_sum_before_umount = forget_nlookup_sum;
    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail;
    }
    for (int i = 0; i < 200 && destroy_count == 0; i++) {
        usleep(10 * 1000);
    }
    if (destroy_count == 0) {
        printf("[FAIL] timed out waiting for FUSE_DESTROY after umount\n");
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    close(fd);
    pthread_join(th, NULL);
    if (destroy_count != 1 || forget_count < forget_count_before_umount ||
        forget_nlookup_sum < forget_sum_before_umount ||
        forget_trace_contains(forget_trace_nodeids, forget_count, 1)) {
        printf("[FAIL] FUSE teardown regressed: forget=%u/%u nlookup=%llu/%llu destroy=%u "
               "root_forget=%d\n",
               forget_count, forget_count_before_umount, (unsigned long long)forget_nlookup_sum,
               (unsigned long long)forget_sum_before_umount, destroy_count,
               forget_trace_contains(forget_trace_nodeids, forget_count, 1));
        rmdir(mp);
        return -1;
    }
    rmdir(mp);
    return 0;

fail:
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_active_directory_parent_survives_lookup_cache_prune() {
    const char *mp = "/tmp/test_fuse_active_parent_prune";
    char parent_path[512];
    char child_path[512];
    char hello_path[512];
    struct stat st;

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t destroy_count = 0;
    volatile uint32_t lookup_count = 0;
    uint32_t lookup_count_before_parent_relookup = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;
    args.destroy_count = &destroy_count;
    args.lookup_count = &lookup_count;
    args.entry_valid_sec = 1;
    args.attr_valid_sec = 1;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(parent_path, sizeof(parent_path), "%s/parent", mp);
    snprintf(child_path, sizeof(child_path), "%s/parent/child", mp);
    snprintf(hello_path, sizeof(hello_path), "%s/hello.txt", mp);
    if (mkdir(parent_path, 0755) != 0 || mkdir(child_path, 0755) != 0) {
        printf("[FAIL] mkdir active parent tree: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (chdir(child_path) != 0) {
        printf("[FAIL] chdir(%s): %s (errno=%d)\n", child_path, strerror(errno), errno);
        goto fail;
    }
    usleep(2000 * 1000);
    if (stat(hello_path, &st) != 0 || !S_ISREG(st.st_mode)) {
        printf("[FAIL] stat(%s) after TTL: %s (errno=%d)\n", hello_path, strerror(errno), errno);
        goto fail_chdir;
    }
    lookup_count_before_parent_relookup = lookup_count;
    if (stat(parent_path, &st) != 0 || !S_ISDIR(st.st_mode)) {
        printf("[FAIL] stat(%s) after prune: %s (errno=%d)\n", parent_path, strerror(errno),
               errno);
        goto fail_chdir;
    }
    if (lookup_count <= lookup_count_before_parent_relookup) {
        printf("[FAIL] parent cache entry was not pruned: before=%u after=%u\n",
               lookup_count_before_parent_relookup, lookup_count);
        goto fail_chdir;
    }
    if (stat("..", &st) != 0 || !S_ISDIR(st.st_mode)) {
        printf("[FAIL] stat(..) after cache prune: %s (errno=%d)\n", strerror(errno), errno);
        goto fail_chdir;
    }

    if (chdir("/") != 0) {
        printf("[FAIL] chdir(/): %s (errno=%d)\n", strerror(errno), errno);
        goto fail_chdir;
    }
    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail_no_umount;
    }
    for (int i = 0; i < 200 && destroy_count == 0; i++) {
        usleep(10 * 1000);
    }
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    if (destroy_count == 0) {
        printf("[FAIL] timed out waiting for FUSE_DESTROY after umount\n");
        return -1;
    }
    return 0;

fail_chdir:
    if (chdir("/") != 0) {
        printf("[FAIL] cleanup chdir(/): %s (errno=%d)\n", strerror(errno), errno);
    }
fail:
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_lookup_self_alias_rejected_and_forgotten() {
    const char *mp = "/tmp/test_fuse_self_alias";
    char parent_path[512];
    char alias_path[512];
    struct stat st;
    uint64_t parent_nodeid = 0;

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t forget_count = 0;
    volatile uint64_t forget_nlookup_sum = 0;
    volatile uint64_t forget_trace_nodeids[32] = {0};
    volatile uint64_t forget_trace_nlookups[32] = {0};

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;
    args.lookup_self_alias = 1;
    args.forget_count = &forget_count;
    args.forget_nlookup_sum = &forget_nlookup_sum;
    args.forget_trace_nodeids = forget_trace_nodeids;
    args.forget_trace_nlookups = forget_trace_nlookups;
    args.forget_trace_capacity = 32;
    args.entry_valid_sec = 60;
    args.attr_valid_sec = 60;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(parent_path, sizeof(parent_path), "%s/parent", mp);
    snprintf(alias_path, sizeof(alias_path), "%s/parent/self_alias", mp);
    if (mkdir(parent_path, 0755) != 0) {
        printf("[FAIL] mkdir(%s): %s (errno=%d)\n", parent_path, strerror(errno), errno);
        goto fail;
    }
    if (stat(parent_path, &st) != 0 || !S_ISDIR(st.st_mode)) {
        printf("[FAIL] stat(%s): %s (errno=%d)\n", parent_path, strerror(errno), errno);
        goto fail;
    }
    parent_nodeid = (uint64_t)st.st_ino;

    errno = 0;
    if (stat(alias_path, &st) == 0 || errno != EIO) {
        printf("[FAIL] self alias lookup expected EIO, ret_errno=%d\n", errno);
        goto fail;
    }
    for (int i = 0; i < 200; i++) {
        if (forget_trace_contains(forget_trace_nodeids, forget_count, parent_nodeid)) {
            break;
        }
        usleep(10 * 1000);
    }
    if (!forget_trace_contains(forget_trace_nodeids, forget_count, parent_nodeid) ||
        forget_nlookup_sum == 0) {
        printf("[FAIL] self alias lookup ref was not forgotten: parent=%llu count=%u sum=%llu\n",
               (unsigned long long)parent_nodeid, forget_count,
               (unsigned long long)forget_nlookup_sum);
        goto fail;
    }

    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_same_generation_type_mismatch_stales_old_node() {
    const char *mp = "/tmp/test_fuse_type_mismatch";
    char file_path[512];
    int old_fd = -1;
    struct stat st;

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.stop_on_destroy = 1;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(file_path, sizeof(file_path), "%s/hello.txt", mp);
    old_fd = open(file_path, O_RDONLY);
    if (old_fd < 0) {
        printf("[FAIL] open old hello: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    char buf[64];
    if (read(old_fd, buf, sizeof(buf)) <= 0) {
        printf("[FAIL] initial read old hello: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    args.fs.nodes[1].is_dir = 1;
    args.fs.nodes[1].mode = S_IFDIR | 0755;
    args.fs.nodes[1].size = 0;
    if (stat(file_path, &st) != 0 || !S_ISDIR(st.st_mode)) {
        printf("[FAIL] stat same-generation replacement dir: %s (errno=%d) mode=%o\n",
               strerror(errno), errno, st.st_mode);
        goto fail;
    }

    errno = 0;
    if (pread(old_fd, buf, sizeof(buf), 0) >= 0 || errno != ESTALE) {
        printf("[FAIL] old fd after type mismatch expected ESTALE, errno=%d\n", errno);
        goto fail;
    }
    close(old_fd);
    old_fd = -1;

    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (old_fd >= 0) {
        close(old_fd);
    }
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_readdir_typed_entries_avoid_n_plus_one_and_preserve_cookies() {
    const char *mp = "/tmp/test_fuse_readdir_typed_entries";
    const char *overlay_root = "/tmp/test_fuse_readdir_typed_overlay";
    char upper[256] = {};
    char work[256] = {};
    char merged[256] = {};
    char overlay_options[1024] = {};
    bool overlay_mounted = false;
    int fuse_fd = -1;
    int dir_fd = -1;
    pthread_t daemon_thread;
    bool daemon_started = false;
    volatile int stop = 0;
    volatile int init_done = 0;
    struct p4_readdir_daemon_args args;
    const char *expected_names[] = {".", "..", "hello.txt", "alpha.txt", "beta.txt"};
    const int64_t expected_cookies[] = {11, 29, 101, 4099, 65537};
    uint64_t overlay_inos[5] = {0};
    uint32_t lookup_before = 0;
    uint32_t getattr_before = 0;
    uint32_t forget_before = 0;
    volatile int readers_ready = 0;
    volatile int start_readers = 0;
    pthread_mutex_t counts_lock = PTHREAD_MUTEX_INITIALIZER;
    unsigned int concurrent_name_counts[5] = {0};
    struct p4_shared_readdir_args reader_args[2];
    pthread_t reader_threads[2];
    size_t readers_started = 0;
    volatile int stop_seeker = 0;
    struct p4_shared_seek_args seeker_args;
    pthread_t seeker_thread;
    bool seeker_started = false;
    size_t seen = 0;
    unsigned char resume_buf[32];
    ssize_t resume_n = -1;
    struct p4_linux_dirent64 *resumed = NULL;
    memset(&args, 0, sizeof(args));

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }
    fuse_fd = open("/dev/fuse", O_RDWR);
    if (fuse_fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }
    args.common.fd = fuse_fd;
    args.common.stop = &stop;
    args.common.init_done = &init_done;
    args.common.stop_on_destroy = 1;
    args.common.force_opendir_enosys = 1;
    args.common.entry_valid_sec = 60;
    args.common.attr_valid_sec = 60;
    args.common.init_out_flags_override =
        FUSE_INIT_EXT | FUSE_MAX_PAGES | FUSE_NO_OPENDIR_SUPPORT;
    if (pthread_create(&daemon_thread, NULL, p4_readdir_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fuse_fd);
        rmdir(mp);
        return -1;
    }
    daemon_started = true;

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fuse_fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        goto fail_no_umount;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }
    dir_fd = open(mp, O_RDONLY | O_DIRECTORY);
    if (dir_fd < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail;
    }

    lookup_before = args.lookup_count;
    getattr_before = args.getattr_count;
    forget_before = args.forget_count;
    memset(&seeker_args, 0, sizeof(seeker_args));
    seeker_args.fd = dir_fd;
    seeker_args.go = &start_readers;
    seeker_args.stop = &stop_seeker;
    if (pthread_create(&seeker_thread, NULL, p4_shared_seek_thread, &seeker_args) != 0) {
        printf("[FAIL] pthread_create shared directory seeker\n");
        goto fail;
    }
    seeker_started = true;
    for (size_t i = 0; i < 2; i++) {
        memset(&reader_args[i], 0, sizeof(reader_args[i]));
        reader_args[i].fd = dir_fd;
        reader_args[i].ready = &readers_ready;
        reader_args[i].go = &start_readers;
        reader_args[i].lock = &counts_lock;
        reader_args[i].name_counts = concurrent_name_counts;
        if (pthread_create(&reader_threads[i], NULL, p4_shared_readdir_thread,
                           &reader_args[i]) != 0) {
            printf("[FAIL] pthread_create shared readdir reader\n");
            goto fail;
        }
        readers_started++;
    }
    while (readers_ready != 2) {
        sched_yield();
    }
    start_readers = 1;
    __sync_synchronize();
    for (size_t i = 0; i < readers_started; i++) {
        pthread_join(reader_threads[i], NULL);
    }
    readers_started = 0;
    stop_seeker = 1;
    __sync_synchronize();
    pthread_join(seeker_thread, NULL);
    seeker_started = false;
    for (size_t i = 0; i < 5; i++) {
        if (reader_args[0].error != 0 || reader_args[1].error != 0 ||
            seeker_args.error != 0 || concurrent_name_counts[i] != 1) {
            printf("[FAIL] shared readdir entry[%zu] count=%u errors=%d/%d seek=%d\n", i,
                   concurrent_name_counts[i], reader_args[0].error, reader_args[1].error,
                   seeker_args.error);
            goto fail;
        }
    }
    if (lseek(dir_fd, 0, SEEK_SET) != 0) {
        printf("[FAIL] reset directory after shared readdir: %s (errno=%d)\n", strerror(errno),
               errno);
        goto fail;
    }
    for (;;) {
        unsigned char buf[32];
        ssize_t n = syscall(SYS_getdents64, dir_fd, buf, sizeof(buf));
        if (n < 0) {
            printf("[FAIL] getdents64 small buffer: %s (errno=%d)\n", strerror(errno), errno);
            goto fail;
        }
        if (n == 0) {
            break;
        }
        if ((size_t)n < offsetof(struct p4_linux_dirent64, d_name)) {
            printf("[FAIL] short getdents64 record: n=%zd\n", n);
            goto fail;
        }
        struct p4_linux_dirent64 *entry = (struct p4_linux_dirent64 *)buf;
        if (entry->d_reclen != (unsigned short)n || seen >= 5 ||
            strcmp(entry->d_name, expected_names[seen]) != 0 ||
            entry->d_off != expected_cookies[seen] ||
            entry->d_ino != p4_mock_dir_entries[seen].ino ||
            entry->d_type != p4_mock_dir_entries[seen].type) {
            printf("[FAIL] dirent[%zu] name=%s off=%lld reclen=%u n=%zd\n", seen,
                   entry->d_name, (long long)entry->d_off, entry->d_reclen, n);
            goto fail;
        }
        seen++;
    }
    if (seen != 5 || args.lookup_count != lookup_before || args.getattr_count != getattr_before ||
        args.readdir_count == 0 || args.readdirplus_count != 0) {
        printf("[FAIL] READDIR typed-entry counters seen=%zu lookup=%u/%u getattr=%u/%u "
               "readdir=%u plus=%u\n",
               seen, lookup_before, args.lookup_count, getattr_before, args.getattr_count,
               args.readdir_count, args.readdirplus_count);
        goto fail;
    }

    if (lseek(dir_fd, 29, SEEK_SET) != 29) {
        printf("[FAIL] seekdir cookie 29: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    errno = 0;
    if (syscall(SYS_getdents64, dir_fd, resume_buf, 24) >= 0 || errno != EINVAL) {
        printf("[FAIL] undersized getdents expected EINVAL without advancing: errno=%d\n", errno);
        goto fail;
    }
    resume_n = syscall(SYS_getdents64, dir_fd, resume_buf, sizeof(resume_buf));
    if (resume_n <= 0) {
        printf("[FAIL] getdents64 after undersized buffer: n=%zd errno=%d\n", resume_n, errno);
        goto fail;
    }
    resumed = (struct p4_linux_dirent64 *)resume_buf;
    if (strcmp(resumed->d_name, "hello.txt") != 0 || resumed->d_off != 101) {
        printf("[FAIL] undersized buffer advanced name=%s off=%lld\n", resumed->d_name,
               (long long)resumed->d_off);
        goto fail;
    }

    if (lseek(dir_fd, 101, SEEK_SET) != 101) {
        printf("[FAIL] seekdir cookie 101: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    resume_n = syscall(SYS_getdents64, dir_fd, resume_buf, sizeof(resume_buf));
    if (resume_n <= 0) {
        printf("[FAIL] getdents64 after cookie seek: n=%zd errno=%d\n", resume_n, errno);
        goto fail;
    }
    resumed = (struct p4_linux_dirent64 *)resume_buf;
    if (strcmp(resumed->d_name, "alpha.txt") != 0 || resumed->d_off != 4099) {
        printf("[FAIL] cookie resume name=%s off=%lld\n", resumed->d_name,
               (long long)resumed->d_off);
        goto fail;
    }

    // Rewinding discards the cached snapshot. A subsequent seek to an opaque
    // cookie must still resume after that cookie when getdents rebuilds it.
    if (lseek(dir_fd, 0, SEEK_SET) != 0 || lseek(dir_fd, 101, SEEK_SET) != 101) {
        printf("[FAIL] seekdir cookie after snapshot reset: %s (errno=%d)\n", strerror(errno),
               errno);
        goto fail;
    }
    errno = 0;
    if (syscall(SYS_getdents64, dir_fd, resume_buf, 24) >= 0 || errno != EINVAL) {
        printf("[FAIL] snapshot-reset seek advanced on undersized getdents: errno=%d\n", errno);
        goto fail;
    }
    resume_n = syscall(SYS_getdents64, dir_fd, resume_buf, sizeof(resume_buf));
    if (resume_n <= 0) {
        printf("[FAIL] getdents64 after snapshot-reset seek: n=%zd errno=%d\n", resume_n, errno);
        goto fail;
    }
    resumed = (struct p4_linux_dirent64 *)resume_buf;
    if (strcmp(resumed->d_name, "alpha.txt") != 0 || resumed->d_off != 4099) {
        printf("[FAIL] snapshot-reset cookie resume name=%s off=%lld\n", resumed->d_name,
               (long long)resumed->d_off);
        goto fail;
    }

    close(dir_fd);
    dir_fd = -1;
    snprintf(upper, sizeof(upper), "%s/upper", overlay_root);
    snprintf(work, sizeof(work), "%s/work", overlay_root);
    snprintf(merged, sizeof(merged), "%s/merged", overlay_root);
    if (ensure_dir(overlay_root) != 0 || ensure_dir(upper) != 0 || ensure_dir(work) != 0 ||
        ensure_dir(merged) != 0) {
        printf("[FAIL] create typed overlay directories: %s (errno=%d)\n", strerror(errno),
               errno);
        goto fail;
    }
    snprintf(overlay_options, sizeof(overlay_options), "lowerdir=%s,upperdir=%s,workdir=%s", mp,
             upper, work);
    if (mount("overlay", merged, "overlay", 0, overlay_options) != 0) {
        printf("[FAIL] mount typed overlay: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    overlay_mounted = true;
    dir_fd = open(merged, O_RDONLY | O_DIRECTORY);
    if (dir_fd < 0) {
        printf("[FAIL] open typed overlay: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    lookup_before = args.lookup_count;
    getattr_before = args.getattr_count;
    forget_before = args.forget_count;
    memset(concurrent_name_counts, 0, sizeof(concurrent_name_counts));
    for (;;) {
        unsigned char overlay_buf[512];
        ssize_t overlay_n = syscall(SYS_getdents64, dir_fd, overlay_buf, sizeof(overlay_buf));
        if (overlay_n < 0) {
            printf("[FAIL] overlay getdents64: %s (errno=%d)\n", strerror(errno), errno);
            goto fail;
        }
        if (overlay_n == 0) {
            break;
        }
        size_t offset = 0;
        while (offset < (size_t)overlay_n) {
            struct p4_linux_dirent64 *entry =
                (struct p4_linux_dirent64 *)(overlay_buf + offset);
            int index = p4_mock_entry_index(entry->d_name);
            if (index < 0 || entry->d_reclen == 0 ||
                offset + entry->d_reclen > (size_t)overlay_n ||
                entry->d_ino == 0 ||
                entry->d_type != p4_mock_dir_entries[index].type) {
                printf("[FAIL] overlay typed dirent name=%s ino=%llu type=%u\n", entry->d_name,
                       (unsigned long long)entry->d_ino, entry->d_type);
                goto fail;
            }
            concurrent_name_counts[index]++;
            overlay_inos[index] = entry->d_ino;
            offset += entry->d_reclen;
        }
    }
    for (size_t i = 0; i < 5; i++) {
        if (concurrent_name_counts[i] != 1) {
            printf("[FAIL] overlay typed entry[%zu] count=%u\n", i,
                   concurrent_name_counts[i]);
            goto fail;
        }
    }
    if (args.lookup_count != lookup_before || args.getattr_count != getattr_before ||
        args.forget_count != forget_before) {
        printf("[FAIL] overlay readdir issued N+1 lookup=%u/%u getattr=%u/%u forget=%u/%u\n",
               lookup_before, args.lookup_count, getattr_before, args.getattr_count,
               forget_before, args.forget_count);
        goto fail;
    }
    for (size_t i = 2; i < 5; i++) {
        struct stat overlay_stat;
        if (fstatat(dir_fd, expected_names[i], &overlay_stat, AT_SYMLINK_NOFOLLOW) != 0 ||
            (uint64_t)overlay_stat.st_ino != overlay_inos[i]) {
            printf("[FAIL] overlay ino mismatch name=%s dirent=%llu stat=%llu errno=%d\n",
                   expected_names[i], (unsigned long long)overlay_inos[i],
                   (unsigned long long)overlay_stat.st_ino, errno);
            goto fail;
        }
    }
    close(dir_fd);
    dir_fd = -1;
    umount(merged);
    overlay_mounted = false;
    rmdir(merged);
    rmdir(work);
    rmdir(upper);
    rmdir(overlay_root);
    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail_no_umount;
    }
    stop = 1;
    close(fuse_fd);
    pthread_join(daemon_thread, NULL);
    rmdir(mp);
    return 0;

fail:
    start_readers = 1;
    __sync_synchronize();
    for (size_t i = 0; i < readers_started; i++) {
        pthread_join(reader_threads[i], NULL);
    }
    stop_seeker = 1;
    __sync_synchronize();
    if (seeker_started) {
        pthread_join(seeker_thread, NULL);
    }
    if (dir_fd >= 0) {
        close(dir_fd);
    }
    if (overlay_mounted) {
        umount(merged);
    }
    rmdir(merged);
    rmdir(work);
    rmdir(upper);
    rmdir(overlay_root);
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fuse_fd);
    if (daemon_started) {
        pthread_join(daemon_thread, NULL);
    }
    rmdir(mp);
    return -1;
}

static int ext_test_readdirplus_auto_uses_plus_only_for_initial_batch() {
    const char *mp = "/tmp/test_fuse_readdirplus_auto";
    int fuse_fd = -1;
    int dir_fd = -1;
    pthread_t daemon_thread;
    bool daemon_started = false;
    volatile int stop = 0;
    volatile int init_done = 0;
    struct p4_readdir_daemon_args args;
    unsigned char buf[512];
    ssize_t n = -1;
    int rounds = 0;
    memset(&args, 0, sizeof(args));

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }
    fuse_fd = open("/dev/fuse", O_RDWR);
    if (fuse_fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }
    args.common.fd = fuse_fd;
    args.common.stop = &stop;
    args.common.init_done = &init_done;
    args.common.stop_on_destroy = 1;
    args.common.force_opendir_enosys = 1;
    args.common.entry_valid_sec = 60;
    args.common.attr_valid_sec = 60;
    args.common.init_out_flags_override = FUSE_INIT_EXT | FUSE_MAX_PAGES |
                                          FUSE_NO_OPENDIR_SUPPORT | FUSE_DO_READDIRPLUS |
                                          FUSE_READDIRPLUS_AUTO;
    args.one_entry_per_reply = 1;
    if (pthread_create(&daemon_thread, NULL, p4_readdir_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fuse_fd);
        rmdir(mp);
        return -1;
    }
    daemon_started = true;

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fuse_fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        goto fail_no_umount;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }
    dir_fd = open(mp, O_RDONLY | O_DIRECTORY);
    if (dir_fd < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail;
    }
    do {
        n = syscall(SYS_getdents64, dir_fd, buf, sizeof(buf));
        if (n < 0) {
            printf("[FAIL] AUTO getdents64: n=%zd errno=%d\n", n, errno);
            goto fail;
        }
        rounds++;
    } while (n > 0 && rounds < 16);
    if (n != 0) {
        printf("[FAIL] AUTO getdents64 did not reach EOF after %d rounds\n", rounds);
        goto fail;
    }
    if (args.readdirplus_count != 1 || args.readdir_count == 0 || args.dir_trace_count < 2 ||
        args.dir_opcode_trace[0] != FUSE_READDIRPLUS || args.dir_offset_trace[0] != 0 ||
        args.dir_opcode_trace[1] != FUSE_READDIR || args.dir_offset_trace[1] == 0) {
        printf("[FAIL] AUTO sequence plus=%u readdir=%u trace=%u first=(%u,%llu) "
               "second=(%u,%llu)\n",
               args.readdirplus_count, args.readdir_count, args.dir_trace_count,
               args.dir_opcode_trace[0], (unsigned long long)args.dir_offset_trace[0],
               args.dir_opcode_trace[1], (unsigned long long)args.dir_offset_trace[1]);
        goto fail;
    }

    close(dir_fd);
    dir_fd = -1;
    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail_no_umount;
    }
    stop = 1;
    close(fuse_fd);
    pthread_join(daemon_thread, NULL);
    rmdir(mp);
    return 0;

fail:
    if (dir_fd >= 0) {
        close(dir_fd);
    }
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fuse_fd);
    if (daemon_started) {
        pthread_join(daemon_thread, NULL);
    }
    rmdir(mp);
    return -1;
}

static int ext_test_readdirplus_generation_mismatch_stales_old_node() {
    const char *mp = "/tmp/test_fuse_readdirplus_generation";
    char file_path[512];
    int old_fd = -1;
    int new_fd = -1;
    DIR *dir = NULL;
    int saw = 0;

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    volatile uint32_t readdirplus_count = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.stop_on_destroy = 1;
    args.readdirplus_count = &readdirplus_count;
    args.force_opendir_enosys = 1;
    args.init_out_flags_override =
        FUSE_INIT_EXT | FUSE_MAX_PAGES | FUSE_NO_OPENDIR_SUPPORT | FUSE_DO_READDIRPLUS;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(file_path, sizeof(file_path), "%s/hello.txt", mp);
    old_fd = open(file_path, O_RDONLY);
    if (old_fd < 0) {
        printf("[FAIL] open old hello: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    char buf[64];
    if (read(old_fd, buf, sizeof(buf)) <= 0) {
        printf("[FAIL] initial read old hello: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    args.fs.nodes[1].generation = 2;
    dir = opendir(mp);
    if (!dir) {
        printf("[FAIL] opendir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail;
    }
    struct dirent *de;
    while ((de = readdir(dir)) != NULL) {
        if (strcmp(de->d_name, "hello.txt") == 0) {
            saw = 1;
        }
    }
    closedir(dir);
    dir = NULL;
    if (!saw || readdirplus_count == 0) {
        printf("[FAIL] expected hello.txt from READDIRPLUS, saw=%d count=%u\n", saw,
               readdirplus_count);
        goto fail;
    }

    errno = 0;
    if (pread(old_fd, buf, sizeof(buf), 0) >= 0) {
        printf("[FAIL] stale old fd read unexpectedly succeeded\n");
        goto fail;
    }
    close(old_fd);
    old_fd = -1;

    new_fd = open(file_path, O_RDONLY);
    if (new_fd < 0) {
        printf("[FAIL] open fresh hello: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (read(new_fd, buf, sizeof(buf)) <= 0) {
        printf("[FAIL] read fresh hello: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    close(new_fd);
    new_fd = -1;

    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (dir) {
        closedir(dir);
    }
    if (new_fd >= 0) {
        close(new_fd);
    }
    if (old_fd >= 0) {
        close(old_fd);
    }
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_create_generation_mismatch_stales_old_node() {
    const char *mp = "/tmp/test_fuse_create_generation";
    char old_path[512];
    char new_path[512];
    int old_fd = -1;
    int new_fd = -1;

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(old_path, sizeof(old_path), "%s/hello.txt", mp);
    snprintf(new_path, sizeof(new_path), "%s/reused.txt", mp);
    old_fd = open(old_path, O_RDONLY);
    if (old_fd < 0) {
        printf("[FAIL] open old hello: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (unlink(old_path) != 0) {
        printf("[FAIL] unlink old hello: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    args.create_reuse_nodeid = 2;
    args.create_generation_override = 2;
    new_fd = open(new_path, O_CREAT | O_RDWR, 0644);
    if (new_fd < 0) {
        printf("[FAIL] create reused node: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    char buf[64];
    errno = 0;
    if (pread(old_fd, buf, sizeof(buf), 0) >= 0) {
        printf("[FAIL] stale old fd after create unexpectedly succeeded\n");
        goto fail;
    }
    close(old_fd);
    old_fd = -1;
    close(new_fd);
    new_fd = -1;

    if (unlink(new_path) != 0) {
        printf("[FAIL] unlink reused node: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (new_fd >= 0) {
        close(new_fd);
    }
    if (old_fd >= 0) {
        close(old_fd);
    }
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_link_generation_mismatch_stales_old_node() {
    const char *mp = "/tmp/test_fuse_link_generation";
    char old_path[512];
    char hard_path[512];
    int old_fd = -1;

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;
    args.link_reuse_old_nodeid = 1;
    args.link_generation_override = 2;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(old_path, sizeof(old_path), "%s/hello.txt", mp);
    snprintf(hard_path, sizeof(hard_path), "%s/hard.txt", mp);
    old_fd = open(old_path, O_RDONLY);
    if (old_fd < 0) {
        printf("[FAIL] open old hello: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (link(old_path, hard_path) != 0) {
        printf("[FAIL] link reused node: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    char buf[64];
    errno = 0;
    if (pread(old_fd, buf, sizeof(buf), 0) >= 0) {
        printf("[FAIL] stale old fd after link unexpectedly succeeded\n");
        goto fail;
    }
    close(old_fd);
    old_fd = -1;

    unlink(hard_path);
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (old_fd >= 0) {
        close(old_fd);
    }
    unlink(hard_path);
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_rename_replace_clears_old_target_path() {
    const char *mp = "/tmp/test_fuse_rename_replace";
    char old_path[512];
    char victim_path[512];
    char cwd[512];
    int old_fd = -1;
    int victim_fd = -1;

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;
    args.allow_rename_replace = 1;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(old_path, sizeof(old_path), "%s/old_dir", mp);
    snprintf(victim_path, sizeof(victim_path), "%s/victim_dir", mp);
    if (mkdir(old_path, 0755) != 0 || mkdir(victim_path, 0755) != 0) {
        printf("[FAIL] mkdir rename-replace dirs: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    old_fd = open(old_path, O_RDONLY | O_DIRECTORY);
    victim_fd = open(victim_path, O_RDONLY | O_DIRECTORY);
    if (old_fd < 0 || victim_fd < 0) {
        printf("[FAIL] open rename-replace dirs: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (rename(old_path, victim_path) != 0) {
        printf("[FAIL] rename replace: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (fchdir(old_fd) != 0 || !getcwd(cwd, sizeof(cwd)) || strcmp(cwd, victim_path) != 0) {
        printf("[FAIL] source fd path after rename replace: cwd='%s' errno=%d (%s)\n", cwd, errno,
               strerror(errno));
        goto fail_chdir_root;
    }
    if (chdir("/") != 0) {
        printf("[FAIL] chdir(/): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    errno = 0;
    if (fchdir(victim_fd) == 0) {
        printf("[FAIL] replaced target fd still resolved to a path\n");
        goto fail_chdir_root;
    }
    close(old_fd);
    close(victim_fd);
    old_fd = -1;
    victim_fd = -1;

    rmdir(victim_path);
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail_chdir_root:
    {
        int ignored_chdir = chdir("/");
        (void)ignored_chdir;
    }
fail:
    if (old_fd >= 0) {
        close(old_fd);
    }
    if (victim_fd >= 0) {
        close(victim_fd);
    }
    rmdir(victim_path);
    rmdir(old_path);
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

TEST(FuseExtended, OpsAccessCreateSymlinkLinkRename2FlushFsync) {
    ASSERT_EQ(0, ext_test_p2_ops());
}

TEST(FuseExtended, DirtyMultibatchNotifyInvalidation) {
    ASSERT_EQ(0, ext_test_dirty_multibatch_notify_invalidation());
}

TEST(FuseExtended, PositiveLookupCacheRespectsEntryTtl) {
    ASSERT_EQ(0, ext_test_positive_lookup_cache_respects_entry_ttl());
}

TEST(FuseExtended, XattrOps) {
    ASSERT_EQ(0, ext_test_xattr_ops());
}

TEST(FuseExtended, XattrEnosysIsCached) {
    ASSERT_EQ(0, ext_test_xattr_enosys_is_cached());
}

TEST(FuseExtended, InterruptDeliversFuseInterrupt) {
    ASSERT_EQ(0, ext_test_p3_interrupt());
}

TEST(FuseExtended, NoOpenNoOpendirReaddirplusNotify) {
    ASSERT_EQ(0, ext_test_p3_noopen_readdirplus_notify());
}

TEST(FuseExtended, OpenReturnsZeroFhIsValid) {
    ASSERT_EQ(0, ext_test_open_zero_fh_valid());
}

TEST(FuseExtended, LargeReadSplitsOverMaxWrite) {
    ASSERT_EQ(0, ext_test_large_read_over_max_write());
}

TEST(FuseExtended, CachedReadUsesOpenFhWithoutExtraOpen) {
    ASSERT_EQ(0, ext_test_cached_read_uses_open_fh_without_extra_open());
}

TEST(FuseExtended, CachedShortReadUpdatesEof) {
    ASSERT_EQ(0, ext_test_cached_short_read_updates_eof());
}

TEST(FuseExtended, ShortReadDiscardsOldPagesAfterRegrow) {
    ASSERT_EQ(0, ext_test_short_read_discards_old_pages_after_regrow());
}

TEST(FuseExtended, CachedReadSeesWriteThroughUpdate) {
    ASSERT_EQ(0, ext_test_cached_read_sees_write_through_update());
}

TEST(FuseExtended, MmapSeesWriteThroughUpdate) {
    ASSERT_EQ(0, ext_test_mmap_sees_write_through_update());
}

TEST(FuseExtended, MmapFaultUsesOpenFhWithoutExtraOpen) {
    ASSERT_EQ(0, ext_test_mmap_fault_uses_open_fh_without_extra_open());
}

TEST(FuseExtended, MmapFaultBatchesReadaroundPages) {
    ASSERT_EQ(0, ext_test_mmap_fault_batches_readaround_pages());
}

TEST(FuseExtended, DirectIoReadBypassesPageCache) {
    ASSERT_EQ(0, ext_test_direct_io_read_bypasses_page_cache());
}

TEST(FuseExtended, DirectIoWriteInvalidatesCachedRead) {
    ASSERT_EQ(0, ext_test_direct_io_write_invalidates_cached_read());
}

TEST(FuseExtended, DirectIoMmapPolicy) {
    ASSERT_EQ(0, ext_test_direct_io_mmap_policy());
}

TEST(FuseExtended, SharedWritableMmapMsyncWriteback) {
    ASSERT_EQ(0, ext_test_shared_writable_mmap_msync_writeback());
}

TEST(FuseExtended, MmapWritebackRetryWaitsOutsideMmGuard) {
    ASSERT_EQ(0, ext_test_mmap_writeback_retry_outside_mm_guard());
}

TEST(FuseExtended, BeyondEofDirtyPageRetiresWithoutWrite) {
    ASSERT_EQ(0, ext_test_beyond_eof_dirty_page_retires());
}

TEST(FuseExtended, SharedMmapDirtyThenPwriteKeepsLatestData) {
    ASSERT_EQ(0, ext_test_shared_mmap_dirty_then_pwrite_keeps_latest_data());
}

TEST(FuseExtended, SharedWritableMmapOSyncWriteback) {
    ASSERT_EQ(0, ext_test_shared_writable_mmap_osync_writeback());
}

TEST(FuseExtended, SharedMmapMprotectWriteback) {
    ASSERT_EQ(0, ext_test_shared_mmap_mprotect_writeback());
}

TEST(FuseExtended, SharedMmapReadonlyFdMprotectWriteDenied) {
    ASSERT_EQ(0, ext_test_shared_mmap_readonly_fd_mprotect_write_denied());
}

TEST(FuseExtended, SharedWritableMmapMunmapWritebackWithoutMsync) {
    ASSERT_EQ(0, ext_test_shared_writable_mmap_munmap_writeback_without_msync());
}

TEST(FuseExtended, SharedMmapSubrangeMprotectWritebackPreservesVma) {
    ASSERT_EQ(0, ext_test_shared_mmap_subrange_mprotect_writeback_preserves_vma());
}

TEST(FuseExtended, SharedMmapUnfaultedMprotectProtNone) {
    ASSERT_EQ(0, ext_test_shared_mmap_unfaulted_mprotect_prot_none());
}

TEST(FuseExtended, MmapTruncateUnmapsStalePage) {
    ASSERT_EQ(0, ext_test_mmap_truncate_unmaps_stale_page());
}

TEST(FuseExtended, FadviseWithoutPageCacheSucceeds) {
    ASSERT_EQ(0, ext_test_fadvise_without_page_cache());
}

TEST(FuseExtended, MountRamfsOnFuseDirectoryUsesNamespacePath) {
    ASSERT_EQ(0, ext_test_mount_on_fuse_dir_uses_namespace_path());
}

TEST(FuseExtended, LookupNodesForgottenBeforeUmountWhenUnreferenced) {
    ASSERT_EQ(0, ext_test_lookup_nodes_forgotten_before_umount_when_unreferenced());
}

TEST(FuseExtended, PositiveLookupCacheExpiresAndForgetsBeforeUmount) {
    ASSERT_EQ(0, ext_test_positive_lookup_cache_expires_and_forgets_before_umount());
}

TEST(FuseExtended, ActiveDirectoryParentSurvivesLookupCachePrune) {
    ASSERT_EQ(0, ext_test_active_directory_parent_survives_lookup_cache_prune());
}

TEST(FuseExtended, LookupSelfAliasRejectedAndForgotten) {
    ASSERT_EQ(0, ext_test_lookup_self_alias_rejected_and_forgotten());
}

TEST(FuseExtended, RenameUpdatesFuseDirectoryCwdPath) {
    ASSERT_EQ(0, ext_test_rename_updates_fuse_dir_cwd_path());
}

TEST(FuseExtended, ReaddirplusGenerationMismatchStalesOldNode) {
    ASSERT_EQ(0, ext_test_readdirplus_generation_mismatch_stales_old_node());
}

TEST(FuseExtended, ReaddirTypedEntriesAvoidNPlusOneAndPreserveCookies) {
    ASSERT_EQ(0, ext_test_readdir_typed_entries_avoid_n_plus_one_and_preserve_cookies());
}

TEST(FuseExtended, ReaddirplusAutoUsesPlusOnlyForInitialBatch) {
    ASSERT_EQ(0, ext_test_readdirplus_auto_uses_plus_only_for_initial_batch());
}

TEST(FuseExtended, SameGenerationTypeMismatchStalesOldNode) {
    ASSERT_EQ(0, ext_test_same_generation_type_mismatch_stales_old_node());
}

TEST(FuseExtended, CreateGenerationMismatchStalesOldNode) {
    ASSERT_EQ(0, ext_test_create_generation_mismatch_stales_old_node());
}

TEST(FuseExtended, LinkGenerationMismatchStalesOldNode) {
    ASSERT_EQ(0, ext_test_link_generation_mismatch_stales_old_node());
}

TEST(FuseExtended, RenameReplaceClearsOldTargetPath) {
    ASSERT_EQ(0, ext_test_rename_replace_clears_old_target_path());
}

TEST(FuseExtended, NoOpenFsyncUsesZeroFh) {
    ASSERT_EQ(0, ext_test_noopen_fsync_uses_zero_fh());
}

TEST(FuseExtended, NoOpenCloseFlushesDirtyDataWithZeroFh) {
    ASSERT_EQ(0, ext_test_noopen_close_flushes_dirty_data_with_zero_fh());
}

TEST(FuseExtended, FsyncEnosysCachedSuccess) {
    ASSERT_EQ(0, ext_test_fsync_enosys_cached_success());
}

TEST(FuseExtended, OpenFlagsMatchLinuxMask) {
    ASSERT_EQ(0, ext_test_open_release_flags_match_linux());
}

TEST(FuseExtended, CreateReusesFuseHandleWithoutOpen) {
    ASSERT_EQ(0, ext_test_create_reuses_fuse_handle());
}

TEST(FuseExtended, CreateEnosysFallsBackAndCaches) {
    ASSERT_EQ(0, ext_test_create_enosys_falls_back_and_caches());
}

TEST(FuseExtended, InvalidCreateReplyCleansResources) {
    ASSERT_EQ(0, ext_test_invalid_create_reply_cleans_resources());
}

TEST(FuseExtended, FsetflUpdatesFuseIoFlags) {
    ASSERT_EQ(0, ext_test_fsetfl_updates_fuse_io_flags());
}

TEST(FuseExtended, FsetflUpdatesFuseDevNonblock) {
    ASSERT_EQ(0, ext_test_fsetfl_updates_fuse_dev_nonblock());
}

TEST(FuseExtended, FopenNoFlushSkipsFlush) {
    ASSERT_EQ(0, ext_test_fopen_noflush_skips_flush());
}

TEST(FuseExtended, WritebackCacheNoFlushStillFlushes) {
    ASSERT_EQ(0, ext_test_writeback_cache_noflush_still_flushes());
}

TEST(FuseExtended, NonWritebackMmapCloseFlushesDirtyMapping) {
    ASSERT_EQ(0, ext_test_nonwriteback_mmap_close_flushes_dirty_mapping());
}

TEST(FuseExtended, CloseReturnsFlushErrorAndClosesFd) {
    ASSERT_EQ(0, ext_test_close_returns_flush_error_and_closes_fd());
}

TEST(FuseExtended, FlushEnosysCachedSuccess) {
    ASSERT_EQ(0, ext_test_flush_enosys_cached_success());
}

TEST(FuseExtended, FopenNonseekableDisablesRandomIo) {
    ASSERT_EQ(0,
              ext_test_fopen_nonseekable_mode(FOPEN_NONSEEKABLE, "/tmp/test_fuse_nonseek", 0));
}

TEST(FuseExtended, FopenStreamDisablesRandomIo) {
    ASSERT_EQ(0, ext_test_fopen_nonseekable_mode(FOPEN_STREAM, "/tmp/test_fuse_stream", 1));
}

TEST(FuseExtended, FopenNonseekableDirectoryDisablesLseek) {
    ASSERT_EQ(0,
              ext_test_fopen_nonseekable_dir_mode(FOPEN_NONSEEKABLE, "/tmp/test_fuse_dir_nonseek"));
}

TEST(FuseExtended, AtomicOTruncUsesOpenWithoutSetattr) {
    ASSERT_EQ(0, ext_test_atomic_otrunc_uses_open_without_setattr());
}

TEST(FuseExtended, FtruncateSetattrUsesOpenFh) {
    ASSERT_EQ(0, ext_test_ftruncate_setattr_uses_open_fh());
}

TEST(FuseExtended, InitRequestsLinuxNoOpenSupport) {
    ASSERT_EQ(0, ext_test_init_requests_linux_no_open_support());
}

TEST(FuseExtended, SubtypeMountFuseDotSubtype) {
    ASSERT_EQ(0, ext_test_p4_subtype_mount());
}

TEST(FuseExtended, PermissionModelAllowOtherDefaultPermissions) {
    if (geteuid() != 0) {
        GTEST_SKIP() << "requires root to execute setuid/setgid permission cases";
    }
    ASSERT_EQ(0, ext_test_permissions());
}

TEST(FuseExtended, DevCloneAttachAndServe) {
    ASSERT_EQ(0, ext_test_clone());
}

TEST(FuseExtended, CachedReadPipelinesRequests) {
    ASSERT_EQ(0, ext_test_cached_read_pipelines_requests());
}

TEST(FuseExtended, CachedReadWithoutAsyncIsSerial) {
    ASSERT_EQ(0, ext_test_cached_read_without_async_is_serial());
}

TEST(FuseExtended, CachedReadSyncErrorSemantics) {
    ASSERT_EQ(0, ext_test_cached_read_sync_error_semantics());
}

int main(int argc, char **argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
