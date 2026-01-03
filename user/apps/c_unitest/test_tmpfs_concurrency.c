/*
 * tmpfs 并发空间计数测试
 *
 * 目的：验证在并发多线程读写 tmpfs 时，df -h 看到的空间大小不会出现问题
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdarg.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/statvfs.h>
#include <time.h>
#include <unistd.h>

#define TEST_DIR "/tmp/tmpfs_test"
#define NUM_THREADS 8
#define NUM_FILES_PER_THREAD 2
#define MAX_FILE_SIZE (4 * 1024 * 1024)  /* 4MB */
#define MIN_FILE_SIZE (256 * 1024)       /* 256KB */
#define TEST_DURATION_SEC 30
#define CHECK_INTERVAL_MS 100

static volatile int g_running = 1;
static volatile unsigned long long g_total_bytes_written = 0;
static volatile unsigned long long g_total_bytes_deleted = 0;
static unsigned long long g_max_used_seen = 0;
static unsigned long long g_min_used_seen = (unsigned long long)-1;
static int g_error_count = 0;

static pthread_mutex_t g_log_mutex = PTHREAD_MUTEX_INITIALIZER;

// 测试辅助函数
static void test_assert(int condition, const char *message) {
    if (!condition) {
        printf("[FAIL] %s\n", message);
        g_error_count++;
    }
}

static void test_success(const char *message) {
    printf("[PASS] %s\n", message);
}

static void log_msg(const char *fmt, ...) {
    va_list args;
    time_t now;
    char timestamp[64];

    time(&now);
    strftime(timestamp, sizeof(timestamp), "%H:%M:%S", localtime(&now));

    pthread_mutex_lock(&g_log_mutex);
    printf("[%s] ", timestamp);
    va_start(args, fmt);
    vprintf(fmt, args);
    va_end(args);
    fflush(stdout);
    pthread_mutex_unlock(&g_log_mutex);
}

/* 获取 tmpfs 的空间使用情况 */
static int get_tmpfs_usage(unsigned long long *total,
                           unsigned long long *used,
                           unsigned long long *available) {
    struct statvfs stat;

    if (statvfs(TEST_DIR, &stat) != 0) {
        log_msg("ERROR: statvfs failed: %s\n", strerror(errno));
        return -1;
    }

    unsigned long long frsize = stat.f_frsize;
    *total = stat.f_blocks * frsize;
    *used = (stat.f_blocks - stat.f_bfree) * frsize;
    *available = stat.f_bavail * frsize;

    return 0;
}

/* 检查空间计数的合理性 */
static int check_space_reasonable(unsigned long long used,
                                  unsigned long long total) {
    /* 检查是否有明显的回绕（used > total）*/
    if (used > total) {
        log_msg("ERROR: Used space (%llu) > Total space (%llu) - possible wraparound!\n",
                used, total);
        test_assert(0, "Space usage exceeds total (wraparound)");
        return -1;
    }

    return 0;
}

/* 定期检查空间使用情况的线程 */
static void *monitor_thread(void *arg) {
    unsigned long long total, used, available;

    (void)arg;
    log_msg("Monitor thread started\n");

    while (g_running) {
        if (get_tmpfs_usage(&total, &used, &available) == 0) {
            /* 更新最大/最小值 */
            if (used > g_max_used_seen) {
                g_max_used_seen = used;
            }
            if (used < g_min_used_seen) {
                g_min_used_seen = used;
            }

            /* 检查合理性 */
            check_space_reasonable(used, total);

            /* 每5秒打印一次状态 */
            static int print_counter = 0;
            if (++print_counter >= (5000 / CHECK_INTERVAL_MS)) {
                log_msg("Space: Used=%.2f MB, Total=%.2f MB | Written=%.2f MB, Deleted=%.2f MB\n",
                        used / 1024.0 / 1024.0,
                        total / 1024.0 / 1024.0,
                        g_total_bytes_written / 1024.0 / 1024.0,
                        g_total_bytes_deleted / 1024.0 / 1024.0);
                print_counter = 0;
            }
        }

        usleep(CHECK_INTERVAL_MS * 1000);
    }

    log_msg("Monitor thread stopped\n");
    return NULL;
}

/* 执行随机文件操作 */
static void *worker_thread(void *arg) {
    unsigned int seed = time(NULL) ^ (unsigned long)(pthread_self());
    int thread_id = *(int *)arg;
    char filename[256];
    char *buffer;
    int i;

    buffer = malloc(64 * 1024);
    if (!buffer) {
        log_msg("Thread %d: Failed to allocate buffer\n", thread_id);
        return NULL;
    }

    /* 用随机数据填充缓冲区 */
    for (i = 0; i < 64 * 1024 / sizeof(int); i++) {
        ((int *)buffer)[i] = rand_r(&seed);
    }

    log_msg("Thread %d started\n", thread_id);

    while (g_running) {
        /* 随机选择一个文件 */
        int file_idx = rand_r(&seed) % NUM_FILES_PER_THREAD;
        snprintf(filename, sizeof(filename), "%s/file_%d_%d.dat",
                 TEST_DIR, thread_id, file_idx);

        /* 随机选择操作类型 */
        int op = rand_r(&seed) % 100;

        if (op < 30) {
            /* 写入/创建文件 (30%) */
            size_t size = MIN_FILE_SIZE +
                          (rand_r(&seed) % (MAX_FILE_SIZE - MIN_FILE_SIZE));
            size_t chunk_size = 64 * 1024;
            size_t remaining = size;
            int fd;

            fd = open(filename, O_WRONLY | O_CREAT | O_TRUNC, 0644);
            if (fd >= 0) {
                while (remaining > 0) {
                    size_t to_write = (remaining < chunk_size) ?
                                      remaining : chunk_size;
                    ssize_t n = write(fd, buffer, to_write);
                    if (n < 0) {
                        /* ENOSPC 是正常的空间不足情况 */
                        if (errno != ENOSPC) {
                            log_msg("Thread %d: write failed: %s\n",
                                    thread_id, strerror(errno));
                        }
                        break;
                    }
                    remaining -= n;
                    g_total_bytes_written += n;
                }
                close(fd);
            }

        } else if (op < 60) {
            /* Truncate 文件 (30%) */
            size_t new_size;
            int fd, op_type = rand_r(&seed) % 3;

            fd = open(filename, O_WRONLY | O_CREAT, 0644);
            if (fd >= 0) {
                struct stat st;

                if (fstat(fd, &st) == 0) {
                    size_t current_size = st.st_size;

                    switch (op_type) {
                    case 0: /* 扩展 */
                        new_size = current_size + (rand_r(&seed) % 1024 * 1024);
                        break;
                    case 1: /* 缩小 */
                        new_size = (current_size > 0) ?
                                   (rand_r(&seed) % current_size) : 0;
                        break;
                    default: /* 随机大小 */
                        new_size = rand_r(&seed) % MAX_FILE_SIZE;
                        break;
                    }

                    if (ftruncate(fd, new_size) == 0) {
                        if (new_size > current_size) {
                            __sync_add_and_fetch(&g_total_bytes_written,
                                                 new_size - current_size);
                        } else {
                            __sync_add_and_fetch(&g_total_bytes_deleted,
                                                 current_size - new_size);
                        }
                    }
                }
                close(fd);
            }

        } else if (op < 90) {
            /* 读取文件 (30%) */
            int fd = open(filename, O_RDONLY);
            if (fd >= 0) {
                char buf[64 * 1024];
                while (read(fd, buf, sizeof(buf)) > 0) {
                    /* 只是读取，不处理内容 */
                }
                close(fd);
            }

        } else {
            /* 删除文件 (10%) */
            struct stat st;
            if (stat(filename, &st) == 0) {
                unsigned long long file_size = st.st_size;
                if (unlink(filename) == 0) {
                    __sync_add_and_fetch(&g_total_bytes_deleted, file_size);
                }
            }
        }

        /* 随机延迟 */
        usleep(rand_r(&seed) % 10000);
    }

    free(buffer);
    log_msg("Thread %d stopped\n", thread_id);
    return NULL;
}

/* 清理测试目录 */
static void cleanup_test_dir(void) {
    char cmd[512];
    snprintf(cmd, sizeof(cmd), "rm -rf %s", TEST_DIR);
    system(cmd);
}

/* 信号处理 */
static void sig_handler(int sig) {
    (void)sig;
    log_msg("Stopping...\n");
    g_running = 0;
}

int main(int argc, char *argv[]) {
    pthread_t threads[NUM_THREADS];
    pthread_t monitor;
    int thread_ids[NUM_THREADS];
    unsigned long long total, used, available;
    int i, duration = TEST_DURATION_SEC;

    /* 处理命令行参数 */
    if (argc > 1) {
        duration = atoi(argv[1]);
        if (duration <= 0) {
            duration = TEST_DURATION_SEC;
        }
    }

    printf("\n=== tmpfs 并发空间计数测试 ===\n");
    printf("测试目录: %s\n", TEST_DIR);
    printf("线程数: %d\n", NUM_THREADS);
    printf("测试时长: %d 秒\n", duration);
    /* 设置信号处理 */
    signal(SIGINT, sig_handler);
    signal(SIGTERM, sig_handler);

    /* 创建测试目录 */
    cleanup_test_dir();
    if (mkdir(TEST_DIR, 0755) != 0 && errno != EEXIST) {
        printf("ERROR: Failed to create test directory: %s\n", strerror(errno));
        return 1;
    }

    /* 获取初始空间状态 */
    if (get_tmpfs_usage(&total, &used, &available) == 0) {
        printf("初始空间: Used=%.2f MB, Total=%.2f MB\n",
                used / 1024.0 / 1024.0, total / 1024.0 / 1024.0);
    }

    /* 启动监控线程 */
    if (pthread_create(&monitor, NULL, monitor_thread, NULL) != 0) {
        printf("ERROR: Failed to create monitor thread\n");
        cleanup_test_dir();
        return 1;
    }

    /* 启动工作线程 */
    for (i = 0; i < NUM_THREADS; i++) {
        thread_ids[i] = i;
        if (pthread_create(&threads[i], NULL, worker_thread,
                           &thread_ids[i]) != 0) {
            printf("ERROR: Failed to create thread %d\n", i);
            g_running = 0;
            break;
        }
    }

    /* 等待指定时长 */
    printf("测试进行中，按 Ctrl+C 提前终止...\n\n");
    sleep(duration);
    g_running = 0;

    /* 等待所有线程结束 */
    for (i = 0; i < NUM_THREADS; i++) {
        pthread_join(threads[i], NULL);
    }
    pthread_join(monitor, NULL);

    /* 最终检查 */
    printf("\n=== 测试完成 ===\n");

    if (get_tmpfs_usage(&total, &used, &available) == 0) {
        printf("最终空间: Used=%.2f MB, Total=%.2f MB\n",
                used / 1024.0 / 1024.0, total / 1024.0 / 1024.0);
    }

    /* 清理测试目录 */
    cleanup_test_dir();

    /* 再次检查空间 */
    sleep(1);
    if (get_tmpfs_usage(&total, &used, &available) == 0) {
        printf("清理后空间: Used=%.2f MB, Total=%.2f MB\n",
                used / 1024.0 / 1024.0, total / 1024.0 / 1024.0);

        /* 清理后，使用量应该很小 */
        test_assert(used <= 10 * 1024 * 1024,
                   "Used space after cleanup should be minimal (no leak)");
    }

    /* 打印统计 */
    printf("\n=== 统计信息 ===\n");
    printf("最大使用量: %.2f MB\n", g_max_used_seen / 1024.0 / 1024.0);
    printf("最小使用量: %.2f MB\n", g_min_used_seen / 1024.0 / 1024.0);
    printf("总写入量: %.2f MB\n", g_total_bytes_written / 1024.0 / 1024.0);
    printf("总删除量: %.2f MB\n", g_total_bytes_deleted / 1024.0 / 1024.0);
    printf("错误计数: %d\n", g_error_count);

    if (g_error_count == 0) {
        test_success("tmpfs 并发空间计数测试");
        return 0;
    } else {
        printf("\n[FAIL] 检测到 %d 个问题\n", g_error_count);
        return 1;
    }
}
