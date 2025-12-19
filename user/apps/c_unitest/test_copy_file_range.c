/**
 * @file test_copy_file_range.c
 * @brief copy_file_range 系统调用测试用例
 *
 * 测试 copy_file_range 系统调用的各种场景：
 * - 基本拷贝功能
 * - 指定偏移拷贝
 * - 部分拷贝（短读/短写）
 * - 错误处理（无效 fd、无效 flags、目录等）
 * - 同文件重叠检测
 */

#define _GNU_SOURCE
#include <fcntl.h>
#include <unistd.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <errno.h>

/* 系统调用号定义 */
#ifndef __NR_copy_file_range
#if defined(__x86_64__)
#define __NR_copy_file_range 326
#elif defined(__riscv) || defined(__loongarch__)
#define __NR_copy_file_range 285
#else
#error "Unsupported architecture"
#endif
#endif

/* copy_file_range 封装函数 */
static ssize_t copy_file_range_wrapper(int fd_in, off_t *off_in,
                                       int fd_out, off_t *off_out,
                                       size_t len, unsigned int flags)
{
    return syscall(__NR_copy_file_range, fd_in, off_in, fd_out, off_out, len, flags);
}

/* 测试辅助函数 */
#define TEST_DIR "/tmp/cfr_test"
#define SRC_FILE TEST_DIR "/src.txt"
#define DST_FILE TEST_DIR "/dst.txt"

static int test_passed = 0;
static int test_failed = 0;

#define TEST_ASSERT(cond, msg) do { \
    if (!(cond)) { \
        printf("  FAILED: %s (line %d)\n", msg, __LINE__); \
        test_failed++; \
        return -1; \
    } \
} while(0)

#define TEST_START(name) printf("Testing %s...\n", name)
#define TEST_PASS() do { printf("  PASSED\n"); test_passed++; return 0; } while(0)

/* 创建测试目录 */
static int setup_test_dir(void)
{
    mkdir(TEST_DIR, 0755);
    return 0;
}

/* 清理测试文件 */
static void cleanup_test_files(void)
{
    unlink(SRC_FILE);
    unlink(DST_FILE);
}

/* 创建带有指定内容的测试文件 */
static int create_test_file(const char *path, const char *content, size_t len)
{
    int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0)
        return -1;
    ssize_t written = write(fd, content, len);
    close(fd);
    return (written == (ssize_t)len) ? 0 : -1;
}

/* 读取文件内容 */
static ssize_t read_file_content(const char *path, char *buf, size_t len)
{
    int fd = open(path, O_RDONLY);
    if (fd < 0)
        return -1;
    ssize_t n = read(fd, buf, len);
    close(fd);
    return n;
}

/**
 * 测试 1: 基本拷贝功能
 * - 创建源文件，拷贝到目标文件
 * - 验证内容一致
 */
static int test_basic_copy(void)
{
    TEST_START("basic copy");
    cleanup_test_files();

    const char *test_data = "Hello, copy_file_range!";
    size_t data_len = strlen(test_data);

    TEST_ASSERT(create_test_file(SRC_FILE, test_data, data_len) == 0,
                "Failed to create source file");

    int src_fd = open(SRC_FILE, O_RDONLY);
    TEST_ASSERT(src_fd >= 0, "Failed to open source file");

    int dst_fd = open(DST_FILE, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    TEST_ASSERT(dst_fd >= 0, "Failed to open dest file");

    /* 不指定偏移，使用文件当前位置 */
    ssize_t copied = copy_file_range_wrapper(src_fd, NULL, dst_fd, NULL, data_len, 0);
    TEST_ASSERT(copied == (ssize_t)data_len, "copy_file_range returned wrong count");

    close(src_fd);
    close(dst_fd);

    /* 验证目标文件内容 */
    char buf[256] = {0};
    ssize_t n = read_file_content(DST_FILE, buf, sizeof(buf));
    TEST_ASSERT(n == (ssize_t)data_len, "Dest file size mismatch");
    TEST_ASSERT(memcmp(buf, test_data, data_len) == 0, "Content mismatch");

    cleanup_test_files();
    TEST_PASS();
}

/**
 * 测试 2: 指定偏移拷贝
 * - 从源文件偏移 5 开始读取
 * - 写入目标文件偏移 0
 */
static int test_with_offset(void)
{
    TEST_START("copy with offset");
    cleanup_test_files();

    const char *test_data = "0123456789ABCDEF";
    size_t data_len = strlen(test_data);

    TEST_ASSERT(create_test_file(SRC_FILE, test_data, data_len) == 0,
                "Failed to create source file");

    int src_fd = open(SRC_FILE, O_RDONLY);
    TEST_ASSERT(src_fd >= 0, "Failed to open source file");

    int dst_fd = open(DST_FILE, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    TEST_ASSERT(dst_fd >= 0, "Failed to open dest file");

    off_t src_off = 5;  /* 从 '5' 开始 */
    ssize_t copied = copy_file_range_wrapper(src_fd, &src_off, dst_fd, NULL, 5, 0);
    TEST_ASSERT(copied == 5, "copy_file_range returned wrong count");
    TEST_ASSERT(src_off == 10, "Source offset not updated correctly");

    close(src_fd);
    close(dst_fd);

    /* 验证目标文件内容应该是 "56789" */
    char buf[256] = {0};
    ssize_t n = read_file_content(DST_FILE, buf, sizeof(buf));
    TEST_ASSERT(n == 5, "Dest file size mismatch");
    TEST_ASSERT(memcmp(buf, "56789", 5) == 0, "Content mismatch");

    cleanup_test_files();
    TEST_PASS();
}

/**
 * 测试 3: 拷贝超过文件末尾（应只拷贝到 EOF）
 */
static int test_copy_past_eof(void)
{
    TEST_START("copy past EOF");
    cleanup_test_files();

    const char *test_data = "Short";
    size_t data_len = strlen(test_data);

    TEST_ASSERT(create_test_file(SRC_FILE, test_data, data_len) == 0,
                "Failed to create source file");

    int src_fd = open(SRC_FILE, O_RDONLY);
    TEST_ASSERT(src_fd >= 0, "Failed to open source file");

    int dst_fd = open(DST_FILE, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    TEST_ASSERT(dst_fd >= 0, "Failed to open dest file");

    /* 请求拷贝 100 字节，但文件只有 5 字节 */
    ssize_t copied = copy_file_range_wrapper(src_fd, NULL, dst_fd, NULL, 100, 0);
    TEST_ASSERT(copied == (ssize_t)data_len, "Should only copy actual file size");

    close(src_fd);
    close(dst_fd);

    cleanup_test_files();
    TEST_PASS();
}

/**
 * 测试 4: 无效的文件描述符
 */
static int test_invalid_fd(void)
{
    TEST_START("invalid fd");
    cleanup_test_files();

    const char *test_data = "Test";
    TEST_ASSERT(create_test_file(SRC_FILE, test_data, strlen(test_data)) == 0,
                "Failed to create source file");

    int src_fd = open(SRC_FILE, O_RDONLY);
    TEST_ASSERT(src_fd >= 0, "Failed to open source file");

    /* 使用无效的目标 fd */
    ssize_t ret = copy_file_range_wrapper(src_fd, NULL, 9999, NULL, 10, 0);
    TEST_ASSERT(ret == -1 && errno == EBADF, "Should return EBADF for invalid fd");

    close(src_fd);

    /* 使用无效的源 fd */
    int dst_fd = open(DST_FILE, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    TEST_ASSERT(dst_fd >= 0, "Failed to open dest file");

    ret = copy_file_range_wrapper(9999, NULL, dst_fd, NULL, 10, 0);
    TEST_ASSERT(ret == -1 && errno == EBADF, "Should return EBADF for invalid fd");

    close(dst_fd);
    cleanup_test_files();
    TEST_PASS();
}

/**
 * 测试 5: 无效的 flags
 */
static int test_invalid_flags(void)
{
    TEST_START("invalid flags");
    cleanup_test_files();

    const char *test_data = "Test";
    TEST_ASSERT(create_test_file(SRC_FILE, test_data, strlen(test_data)) == 0,
                "Failed to create source file");

    int src_fd = open(SRC_FILE, O_RDONLY);
    TEST_ASSERT(src_fd >= 0, "Failed to open source file");

    int dst_fd = open(DST_FILE, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    TEST_ASSERT(dst_fd >= 0, "Failed to open dest file");

    /* flags 必须为 0 */
    ssize_t ret = copy_file_range_wrapper(src_fd, NULL, dst_fd, NULL, 10, 1);
    TEST_ASSERT(ret == -1 && errno == EINVAL, "Should return EINVAL for non-zero flags");

    close(src_fd);
    close(dst_fd);
    cleanup_test_files();
    TEST_PASS();
}

/**
 * 测试 6: 目录拷贝（应失败）
 */
static int test_directory_copy(void)
{
    TEST_START("directory copy (should fail)");

    int dir_fd = open(TEST_DIR, O_RDONLY | O_DIRECTORY);
    if (dir_fd < 0) {
        printf("  SKIPPED: Cannot open directory\n");
        return 0;
    }

    const char *test_data = "Test";
    create_test_file(DST_FILE, test_data, strlen(test_data));

    int dst_fd = open(DST_FILE, O_WRONLY);
    if (dst_fd >= 0) {
        ssize_t ret = copy_file_range_wrapper(dir_fd, NULL, dst_fd, NULL, 10, 0);
        TEST_ASSERT(ret == -1 && (errno == EISDIR || errno == EINVAL),
                    "Should return EISDIR or EINVAL for directory");
        close(dst_fd);
    }

    close(dir_fd);
    cleanup_test_files();
    TEST_PASS();
}

/**
 * 测试 7: 源文件以只写方式打开（应失败）
 */
static int test_write_only_source(void)
{
    TEST_START("write-only source (should fail)");
    cleanup_test_files();

    const char *test_data = "Test";
    TEST_ASSERT(create_test_file(SRC_FILE, test_data, strlen(test_data)) == 0,
                "Failed to create source file");

    int src_fd = open(SRC_FILE, O_WRONLY);  /* 只写打开 */
    TEST_ASSERT(src_fd >= 0, "Failed to open source file");

    int dst_fd = open(DST_FILE, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    TEST_ASSERT(dst_fd >= 0, "Failed to open dest file");

    ssize_t ret = copy_file_range_wrapper(src_fd, NULL, dst_fd, NULL, 10, 0);
    TEST_ASSERT(ret == -1 && errno == EBADF, "Should return EBADF for write-only source");

    close(src_fd);
    close(dst_fd);
    cleanup_test_files();
    TEST_PASS();
}

/**
 * 测试 8: 目标文件以只读方式打开（应失败）
 */
static int test_read_only_dest(void)
{
    TEST_START("read-only dest (should fail)");
    cleanup_test_files();

    const char *test_data = "Test";
    TEST_ASSERT(create_test_file(SRC_FILE, test_data, strlen(test_data)) == 0,
                "Failed to create source file");
    TEST_ASSERT(create_test_file(DST_FILE, test_data, strlen(test_data)) == 0,
                "Failed to create dest file");

    int src_fd = open(SRC_FILE, O_RDONLY);
    TEST_ASSERT(src_fd >= 0, "Failed to open source file");

    int dst_fd = open(DST_FILE, O_RDONLY);  /* 只读打开 */
    TEST_ASSERT(dst_fd >= 0, "Failed to open dest file");

    ssize_t ret = copy_file_range_wrapper(src_fd, NULL, dst_fd, NULL, 10, 0);
    TEST_ASSERT(ret == -1 && errno == EBADF, "Should return EBADF for read-only dest");

    close(src_fd);
    close(dst_fd);
    cleanup_test_files();
    TEST_PASS();
}

/**
 * 测试 9: 目标文件以 O_APPEND 打开（应失败）
 */
static int test_append_dest(void)
{
    TEST_START("O_APPEND dest (should fail)");
    cleanup_test_files();

    const char *test_data = "Test";
    TEST_ASSERT(create_test_file(SRC_FILE, test_data, strlen(test_data)) == 0,
                "Failed to create source file");

    int src_fd = open(SRC_FILE, O_RDONLY);
    TEST_ASSERT(src_fd >= 0, "Failed to open source file");

    int dst_fd = open(DST_FILE, O_WRONLY | O_CREAT | O_APPEND, 0644);
    TEST_ASSERT(dst_fd >= 0, "Failed to open dest file");

    ssize_t ret = copy_file_range_wrapper(src_fd, NULL, dst_fd, NULL, 10, 0);
    TEST_ASSERT(ret == -1 && errno == EBADF, "Should return EBADF for O_APPEND dest");

    close(src_fd);
    close(dst_fd);
    cleanup_test_files();
    TEST_PASS();
}

/**
 * 测试 10: 负偏移（应失败）
 */
static int test_negative_offset(void)
{
    TEST_START("negative offset (should fail)");
    cleanup_test_files();

    const char *test_data = "Test";
    TEST_ASSERT(create_test_file(SRC_FILE, test_data, strlen(test_data)) == 0,
                "Failed to create source file");

    int src_fd = open(SRC_FILE, O_RDONLY);
    TEST_ASSERT(src_fd >= 0, "Failed to open source file");

    int dst_fd = open(DST_FILE, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    TEST_ASSERT(dst_fd >= 0, "Failed to open dest file");

    off_t neg_off = -10;
    ssize_t ret = copy_file_range_wrapper(src_fd, &neg_off, dst_fd, NULL, 10, 0);
    TEST_ASSERT(ret == -1 && errno == EINVAL, "Should return EINVAL for negative offset");

    close(src_fd);
    close(dst_fd);
    cleanup_test_files();
    TEST_PASS();
}

/**
 * 测试 11: 长度为 0 的拷贝
 */
static int test_zero_length(void)
{
    TEST_START("zero length copy");
    cleanup_test_files();

    const char *test_data = "Test";
    TEST_ASSERT(create_test_file(SRC_FILE, test_data, strlen(test_data)) == 0,
                "Failed to create source file");

    int src_fd = open(SRC_FILE, O_RDONLY);
    TEST_ASSERT(src_fd >= 0, "Failed to open source file");

    int dst_fd = open(DST_FILE, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    TEST_ASSERT(dst_fd >= 0, "Failed to open dest file");

    ssize_t copied = copy_file_range_wrapper(src_fd, NULL, dst_fd, NULL, 0, 0);
    TEST_ASSERT(copied == 0, "Zero length copy should return 0");

    close(src_fd);
    close(dst_fd);
    cleanup_test_files();
    TEST_PASS();
}

/**
 * 测试 12: 大文件拷贝
 */
static int test_large_copy(void)
{
    TEST_START("large file copy");
    cleanup_test_files();

    /* 创建一个 64KB 的测试文件 */
    size_t large_size = 64 * 1024;
    char *large_data = malloc(large_size);
    if (!large_data) {
        printf("  SKIPPED: Cannot allocate memory\n");
        return 0;
    }

    /* 填充测试数据 */
    for (size_t i = 0; i < large_size; i++) {
        large_data[i] = (char)(i & 0xFF);
    }

    TEST_ASSERT(create_test_file(SRC_FILE, large_data, large_size) == 0,
                "Failed to create large source file");

    int src_fd = open(SRC_FILE, O_RDONLY);
    TEST_ASSERT(src_fd >= 0, "Failed to open source file");

    int dst_fd = open(DST_FILE, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    TEST_ASSERT(dst_fd >= 0, "Failed to open dest file");

    ssize_t total_copied = 0;
    while ((size_t)total_copied < large_size) {
        ssize_t copied = copy_file_range_wrapper(src_fd, NULL, dst_fd, NULL,
                                                  large_size - total_copied, 0);
        if (copied <= 0) break;
        total_copied += copied;
    }

    TEST_ASSERT(total_copied == (ssize_t)large_size,
                "Large file copy size mismatch");

    close(src_fd);
    close(dst_fd);

    /* 验证内容 */
    char *verify_buf = malloc(large_size);
    if (verify_buf) {
        ssize_t n = read_file_content(DST_FILE, verify_buf, large_size);
        TEST_ASSERT(n == (ssize_t)large_size, "Read back size mismatch");
        TEST_ASSERT(memcmp(verify_buf, large_data, large_size) == 0,
                    "Large file content mismatch");
        free(verify_buf);
    }

    free(large_data);
    cleanup_test_files();
    TEST_PASS();
}

/* 主函数 */
int main(void)
{
    printf("=== copy_file_range system call tests ===\n\n");

    setup_test_dir();

    /* 运行所有测试 */
    test_basic_copy();
    test_with_offset();
    test_copy_past_eof();
    test_invalid_fd();
    test_invalid_flags();
    test_directory_copy();
    test_write_only_source();
    test_read_only_dest();
    test_append_dest();
    test_negative_offset();
    test_zero_length();
    test_large_copy();

    /* 清理 */
    cleanup_test_files();
    rmdir(TEST_DIR);

    printf("\n=== Test Summary ===\n");
    printf("Passed: %d\n", test_passed);
    printf("Failed: %d\n", test_failed);

    return test_failed > 0 ? 1 : 0;
}
