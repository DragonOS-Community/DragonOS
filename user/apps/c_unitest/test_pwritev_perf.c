// ==============================================
//
//              本文件用于测试系统调用 pwritev 在使用
//              大量小块数据写入时的性能表现。
//              重点测试 user_access_len() 函数的开销
//
// ==============================================

#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/uio.h>
#include <unistd.h>
#include <time.h>
#include <string.h>
#include <errno.h>

#define TEST_FILE "pwritev_test.dat"
#define NUM_IOV 1000
#define SMALL_DATA_SIZE 64
#define TOTAL_ITERATIONS 100

// 测试用的小数据块
struct test_iovec {
    struct iovec iov[NUM_IOV];
    char data[NUM_IOV][SMALL_DATA_SIZE];
};

// 初始化测试数据
void init_test_data(struct test_iovec *test_vec) {
    for (int i = 0; i < NUM_IOV; i++) {
        // 填充每个小块数据
        snprintf(test_vec->data[i], SMALL_DATA_SIZE, "Block_%04d:abcdefghijklmnopqrstuvwxyz", i);
        test_vec->iov[i].iov_base = test_vec->data[i];
        test_vec->iov[i].iov_len = strlen(test_vec->data[i]);
    }
}

// 性能测试函数
double test_pwritev_performance(int fd, struct test_iovec *test_vec, int iterations) {
    struct timespec start, end;

    clock_gettime(CLOCK_MONOTONIC, &start);

    for (int i = 0; i < iterations; i++) {
        ssize_t written = pwritev(fd, test_vec->iov, NUM_IOV, 0);
        if (written == -1) {
            perror("pwritev failed");
            exit(EXIT_FAILURE);
        }

        // 计算总写入字节数
        size_t total_bytes = 0;
        for (int j = 0; j < NUM_IOV; j++) {
            total_bytes += test_vec->iov[j].iov_len;
        }

        if (written != total_bytes) {
            fprintf(stderr, "Partial write: expected %zu, got %zd\n", total_bytes, written);
            exit(EXIT_FAILURE);
        }
    }

    clock_gettime(CLOCK_MONOTONIC, &end);

    double elapsed = (end.tv_sec - start.tv_sec) +
                    (end.tv_nsec - start.tv_nsec) / 1e9;

    return elapsed;
}

// 对比测试：使用单独的 write 调用
double test_individual_writes_performance(int fd, struct test_iovec *test_vec, int iterations) {
    struct timespec start, end;

    clock_gettime(CLOCK_MONOTONIC, &start);

    for (int i = 0; i < iterations; i++) {
        off_t offset = 0;
        for (int j = 0; j < NUM_IOV; j++) {
            ssize_t written = pwrite(fd, test_vec->iov[j].iov_base,
                                   test_vec->iov[j].iov_len, offset);
            if (written == -1) {
                perror("pwrite failed");
                exit(EXIT_FAILURE);
            }
            if (written != test_vec->iov[j].iov_len) {
                fprintf(stderr, "Partial write in individual test\n");
                exit(EXIT_FAILURE);
            }
            offset += written;
        }
    }

    clock_gettime(CLOCK_MONOTONIC, &end);

    double elapsed = (end.tv_sec - start.tv_sec) +
                    (end.tv_nsec - start.tv_nsec) / 1e9;

    return elapsed;
}


int main(void) {
    struct test_iovec test_vec;

    printf("=== pwritev Performance Test ===\n");
    printf("IOV count: %d\n", NUM_IOV);
    printf("Small data size: %d bytes\n", SMALL_DATA_SIZE);
    printf("Iterations: %d\n", TOTAL_ITERATIONS);
    printf("\n");

    // 初始化测试数据
    init_test_data(&test_vec);

    // 计算总数据大小
    size_t total_data_size = 0;
    for (int i = 0; i < NUM_IOV; i++) {
        total_data_size += test_vec.iov[i].iov_len;
    }
    printf("Total data per pwritev call: %zu bytes\n", total_data_size);
    printf("Total data to write: %zu KB\n",
           (total_data_size * TOTAL_ITERATIONS) / 1024);
    printf("\n");

    // 创建测试文件
    int fd = open(TEST_FILE, O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (fd == -1) {
        perror("Failed to create test file");
        exit(EXIT_FAILURE);
    }

    // 性能测试：pwritev
    printf("Testing pwritev performance...\n");
    double pwritev_time = test_pwritev_performance(fd, &test_vec, TOTAL_ITERATIONS);
    printf("pwritev total time: %.4f seconds\n", pwritev_time);
    printf("pwritev average time per call: %.6f ms\n",
           (pwritev_time * 1000) / TOTAL_ITERATIONS);
    printf("pwritev throughput: %.2f MB/s\n",
           (total_data_size * TOTAL_ITERATIONS) / (pwritev_time * 1024 * 1024));
    printf("\n");

    // 重置文件位置
    if (ftruncate(fd, 0) == -1) {
        perror("Failed to truncate file");
        close(fd);
        exit(EXIT_FAILURE);
    }

    // 性能测试：单独的 write 调用对比
    printf("Testing individual pwrite performance (baseline)...\n");
    double individual_time = test_individual_writes_performance(fd, &test_vec, TOTAL_ITERATIONS);
    printf("Individual pwrite total time: %.4f seconds\n", individual_time);
    printf("Individual pwrite average time per call: %.6f ms\n",
           (individual_time * 1000) / TOTAL_ITERATIONS);
    printf("Individual pwrite throughput: %.2f MB/s\n",
           (total_data_size * TOTAL_ITERATIONS) / (individual_time * 1024 * 1024));
    printf("\n");

    // 性能对比
    double speedup = individual_time / pwritev_time;
    printf("Performance comparison:\n");
    printf("pwritev is %.2fx faster than individual writes\n", speedup);
    printf("pwritev saves %.2f%% time\n", (1 - pwritev_time / individual_time) * 100);
    printf("\n");

    // 清理
    close(fd);
    if (unlink(TEST_FILE) == -1) {
        perror("Failed to remove test file");
    }

    printf("test_pwritev_perf ok\n");
    return 0;
}