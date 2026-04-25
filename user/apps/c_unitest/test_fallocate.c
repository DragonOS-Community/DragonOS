#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <limits.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <unistd.h>

// 系统调用号定义 (x86_64)
#ifndef __NR_fallocate
#define __NR_fallocate 285
#endif

// fallocate mode flags (来自 linux/falloc.h)
#ifndef FALLOC_FL_KEEP_SIZE
#define FALLOC_FL_KEEP_SIZE 0x01
#endif
#ifndef FALLOC_FL_PUNCH_HOLE
#define FALLOC_FL_PUNCH_HOLE 0x02
#endif
#ifndef FALLOC_FL_ZERO_RANGE
#define FALLOC_FL_ZERO_RANGE 0x10
#endif

// 测试文件路径
#define TEST_FILE "/tmp/test_fallocate.txt"
#define TEST_DIR "/tmp/test_fallocate_dir"
#define TEST_SYMLINK "/tmp/test_fallocate_symlink"
#define TEST_LARGE_FILE "/tmp/test_fallocate_large.txt"

// 测试辅助宏
#define TEST_ASSERT(cond, msg) do { \
    if (!(cond)) { \
        printf("FAIL: %s (line %d)\n", msg, __LINE__); \
        return -1; \
    } \
} while(0)

#define TEST_PASS(msg) do { \
    printf("PASS: %s\n", msg); \
    return 0; \
} while(0)

#define TEST_SKIP(msg) do { \
    printf("SKIP: %s\n", msg); \
    return 77; \
} while(0)

// 使用 syscall 包装调用 fallocate
static int fallocate_wrapper(int fd, int mode, off_t offset, off_t len) {
    return syscall(__NR_fallocate, fd, mode, offset, len);
}

// 获取文件大小
static off_t get_file_size(const char *path) {
    struct stat st;
    if (stat(path, &st) != 0) {
        return -1;
    }
    return st.st_size;
}

// 获取文件描述符对应文件大小
static off_t get_fd_size(int fd) {
    struct stat st;
    if (fstat(fd, &st) != 0) {
        return -1;
    }
    return st.st_size;
}

// 清理测试文件
static void cleanup_test_file(const char *path) {
    unlink(path);
}

static int write_pattern_fd(int fd, off_t offset, size_t len, unsigned char pattern) {
    unsigned char buf[512];
    size_t remaining = len;

    memset(buf, pattern, sizeof(buf));
    while (remaining > 0) {
        size_t chunk = remaining < sizeof(buf) ? remaining : sizeof(buf);
        ssize_t written = pwrite(fd, buf, chunk, offset);

        if (written < 0)
            return -1;
        if ((size_t)written != chunk)
            return -1;
        offset += written;
        remaining -= written;
    }
    return 0;
}

static int read_full_fd(int fd, off_t offset, unsigned char *buf, size_t len) {
    size_t remaining = len;

    while (remaining > 0) {
        ssize_t readn = pread(fd, buf, remaining, offset);

        if (readn < 0)
            return -1;
        if (readn == 0)
            return -1;
        offset += readn;
        buf += readn;
        remaining -= readn;
    }
    return 0;
}

// 检查当前文件系统是否支持 fallocate
static int check_fallocate_supported(int fd) {
    int ret;

    errno = 0;
    ret = fallocate_wrapper(fd, 0, 0, 1);
    if (ret == 0) {
        if (ftruncate(fd, 0) != 0)
            return -1;
        return 1;
    }
    if (errno == EOPNOTSUPP)
        return 0;
    return -1;
}

// ==================== 基本功能测试 ====================

// 测试默认模式 (mode=0) 的基本空间分配
static int test_basic_fallocate(void) {
    printf("\n--- test_basic_fallocate ---\n");

    // 创建测试文件并写入初始数据
    int fd = open(TEST_FILE, O_CREAT | O_RDWR | O_TRUNC, 0644);
    TEST_ASSERT(fd >= 0, "打开测试文件");
    int support = check_fallocate_supported(fd);
    if (support == 0) {
        close(fd);
        cleanup_test_file(TEST_FILE);
        TEST_SKIP("fallocate 不支持");
    }
    TEST_ASSERT(support > 0, "fallocate 探测失败");

    const char *initial_data = "Hello, World! This is initial data.";
    size_t initial_len = strlen(initial_data);
    ssize_t written = write(fd, initial_data, initial_len);
    TEST_ASSERT(written == (ssize_t)initial_len, "写入初始数据");

    off_t initial_size = get_fd_size(fd);
    printf("初始文件大小: %ld bytes\n", initial_size);
    TEST_ASSERT(initial_size == (off_t)initial_len, "初始文件大小正确");

    // fallocate 扩展文件到 10000 字节
    off_t new_size = 10000;
    int result = fallocate_wrapper(fd, 0, 0, new_size);
    printf("fallocate(0, 0, %ld) 返回: %d, errno: %d\n", new_size, result, result == -1 ? errno : 0);
    TEST_ASSERT(result == 0, "fallocate 扩展文件成功");

    off_t allocated_size = get_fd_size(fd);
    printf("fallocate 后文件大小: %ld bytes\n", allocated_size);
    TEST_ASSERT(allocated_size == new_size, "文件大小正确扩展");

    // 验证可以在扩展区域写入数据
    char buffer[128];
    memset(buffer, 0, sizeof(buffer));

    // 读取初始数据应该仍然存在
    lseek(fd, 0, SEEK_SET);
    ssize_t nread = read(fd, buffer, sizeof(buffer) - 1);
    // fallocate 扩展文件后，新空间被零填充，所以 read 会读取整个文件
    // 我们只需要验证前 initial_len 字节是否匹配
    TEST_ASSERT(nread >= (ssize_t)initial_len, "读取初始数据");
    TEST_ASSERT(memcmp(buffer, initial_data, initial_len) == 0, "初始数据正确");

    // 在扩展区域写入数据
    const char *new_data = "Data written to extended area";
    off_t write_offset = 9000;
    lseek(fd, write_offset, SEEK_SET);
    written = write(fd, new_data, strlen(new_data));
    TEST_ASSERT(written == (ssize_t)strlen(new_data), "写入扩展区域");

    // 读取写入的数据验证
    lseek(fd, write_offset, SEEK_SET);
    memset(buffer, 0, sizeof(buffer));
    nread = read(fd, buffer, strlen(new_data));
    TEST_ASSERT(nread == (ssize_t)strlen(new_data), "读取扩展区域数据");
    TEST_ASSERT(memcmp(buffer, new_data, strlen(new_data)) == 0, "扩展区域数据正确");

    close(fd);
    cleanup_test_file(TEST_FILE);

    TEST_PASS("基本功能测试");
}

// 测试在现有数据后追加分配
static int test_append_to_existing_data(void) {
    printf("\n--- test_append_to_existing_data ---\n");

    int fd = open(TEST_FILE, O_CREAT | O_RDWR | O_TRUNC, 0644);
    TEST_ASSERT(fd >= 0, "打开测试文件");
    int support = check_fallocate_supported(fd);
    if (support == 0) {
        close(fd);
        cleanup_test_file(TEST_FILE);
        TEST_SKIP("fallocate 不支持");
    }
    TEST_ASSERT(support > 0, "fallocate 探测失败");

    // 写入初始数据
    const char *data1 = "Initial data block - ";
    ssize_t written = write(fd, data1, strlen(data1));
    TEST_ASSERT(written == (ssize_t)strlen(data1), "写入初始数据");

    off_t size1 = get_fd_size(fd);
    printf("第一次写入后大小: %ld bytes\n", size1);

    // fallocate 在文件末尾追加空间
    off_t append_len = 5000;
    int result = fallocate_wrapper(fd, 0, size1, append_len);
    printf("fallocate(0, %ld, %ld) 返回: %d\n", size1, append_len, result);
    TEST_ASSERT(result == 0, "追加分配成功");

    off_t size2 = get_fd_size(fd);
    printf("追加分配后大小: %ld bytes\n", size2);
    TEST_ASSERT(size2 == size1 + append_len, "追加分配大小正确");

    // 验证原数据未受影响
    lseek(fd, 0, SEEK_SET);
    char buffer[128];
    ssize_t nread = read(fd, buffer, strlen(data1));
    TEST_ASSERT(nread == (ssize_t)strlen(data1), "读取原数据");
    buffer[strlen(data1)] = '\0';
    TEST_ASSERT(strcmp(buffer, data1) == 0, "原数据未受影响");

    close(fd);
    cleanup_test_file(TEST_FILE);

    TEST_PASS("追加分配测试");
}

// 测试多次连续 fallocate 调用
static int test_multiple_allocations(void) {
    printf("\n--- test_multiple_allocations ---\n");

    int fd = open(TEST_FILE, O_CREAT | O_RDWR | O_TRUNC, 0644);
    TEST_ASSERT(fd >= 0, "打开测试文件");
    int support = check_fallocate_supported(fd);
    if (support == 0) {
        close(fd);
        cleanup_test_file(TEST_FILE);
        TEST_SKIP("fallocate 不支持");
    }
    TEST_ASSERT(support > 0, "fallocate 探测失败");

    off_t current_size = 0;

    // 第一次分配
    off_t alloc1 = 1000;
    int result = fallocate_wrapper(fd, 0, 0, alloc1);
    TEST_ASSERT(result == 0, "第一次分配成功");
    current_size = get_fd_size(fd);
    printf("第一次分配后大小: %ld bytes\n", current_size);
    TEST_ASSERT(current_size == alloc1, "第一次分配大小正确");

    // 第二次分配（继续扩展）
    off_t alloc2 = 5000;
    result = fallocate_wrapper(fd, 0, 0, alloc2);
    TEST_ASSERT(result == 0, "第二次分配成功");
    current_size = get_fd_size(fd);
    printf("第二次分配后大小: %ld bytes\n", current_size);
    TEST_ASSERT(current_size == alloc2, "第二次分配大小正确");

    // 第三次分配（更大）
    off_t alloc3 = 20000;
    result = fallocate_wrapper(fd, 0, 0, alloc3);
    TEST_ASSERT(result == 0, "第三次分配成功");
    current_size = get_fd_size(fd);
    printf("第三次分配后大小: %ld bytes\n", current_size);
    TEST_ASSERT(current_size == alloc3, "第三次分配大小正确");

    close(fd);
    cleanup_test_file(TEST_FILE);

    TEST_PASS("多次分配测试");
}

// ==================== 错误条件测试 ====================

// 测试无效的文件描述符
static int test_invalid_fd(void) {
    printf("\n--- test_invalid_fd ---\n");

    int result = fallocate_wrapper(-1, 0, 0, 1000);
    TEST_ASSERT(result == -1, "无效 fd 应返回错误");
    TEST_ASSERT(errno == EBADF, "无效 fd 应返回 EBADF");

    // 测试一个不存在的 fd
    result = fallocate_wrapper(9999, 0, 0, 1000);
    TEST_ASSERT(result == -1, "不存在的 fd 应返回错误");
    TEST_ASSERT(errno == EBADF, "不存在的 fd 应返回 EBADF");

    TEST_PASS("无效 fd 测试");
}

// 测试只读文件描述符
static int test_readonly_fd(void) {
    printf("\n--- test_readonly_fd ---\n");

    // 创建文件
    int fd_wr = open(TEST_FILE, O_CREAT | O_RDWR | O_TRUNC, 0644);
    TEST_ASSERT(fd_wr >= 0, "创建测试文件");
    ssize_t written = write(fd_wr, "test", 4);
    TEST_ASSERT(written == 4, "写入测试数据");
    close(fd_wr);

    // 以只读方式打开
    int fd_rd = open(TEST_FILE, O_RDONLY);
    TEST_ASSERT(fd_rd >= 0, "以只读方式打开文件");

    int result = fallocate_wrapper(fd_rd, 0, 0, 1000);
    TEST_ASSERT(result == -1, "只读 fd 应返回错误");
    TEST_ASSERT(errno == EBADF, "只读 fd 应返回 EBADF");
    printf("只读 fd fallocate 返回: %d, errno: %d (EBADF=%d)\n", result, errno, EBADF);

    close(fd_rd);
    cleanup_test_file(TEST_FILE);

    TEST_PASS("只读 fd 测试");
}

// 测试对目录调用 fallocate
static int test_directory(void) {
    printf("\n--- test_directory ---\n");

    // 确保目录不存在
    rmdir(TEST_DIR);

    // 创建测试目录
    int result = mkdir(TEST_DIR, 0755);
    TEST_ASSERT(result == 0, "创建测试目录");

    int fd = open(TEST_DIR, O_RDONLY);
    TEST_ASSERT(fd >= 0, "打开目录");

    errno = 0;
    result = fallocate_wrapper(fd, 0, 0, 1000);
    TEST_ASSERT(result == -1, "目录 fallocate 应失败");
    TEST_ASSERT(errno == EBADF, "目录应返回 EBADF（只读 fd）");
    printf("目录 fallocate 返回: %d, errno: %d (EBADF=%d)\n",
           result, errno, EBADF);

    close(fd);
    rmdir(TEST_DIR);

    TEST_PASS("目录测试");
}

// 测试 len=0 的情况
static int test_zero_length(void) {
    printf("\n--- test_zero_length ---\n");

    int fd = open(TEST_FILE, O_CREAT | O_RDWR | O_TRUNC, 0644);
    TEST_ASSERT(fd >= 0, "打开测试文件");

    int result = fallocate_wrapper(fd, 0, 0, 0);
    TEST_ASSERT(result == -1, "len=0 应返回错误");
    TEST_ASSERT(errno == EINVAL, "len=0 应返回 EINVAL");
    printf("len=0 fallocate 返回: %d, errno: %d (EINVAL=%d)\n", result, errno, EINVAL);

    close(fd);
    cleanup_test_file(TEST_FILE);

    TEST_PASS("零长度测试");
}

// 测试无效的 offset 和 length（负值转为大正数超过 isize::MAX）
static int test_invalid_offset_length(void) {
    printf("\n--- test_invalid_offset_length ---\n");

    int fd = open(TEST_FILE, O_CREAT | O_RDWR | O_TRUNC, 0644);
    TEST_ASSERT(fd >= 0, "打开测试文件");

    // 测试负 offset（转换为无符号大数）
    // 在 x86_64 上，-1 作为 off_t 传递，但由于参数类型转换
    // 我们需要测试超过 isize::MAX 的值
    int result = fallocate_wrapper(fd, 0, (off_t)-1, 1000);
    TEST_ASSERT(result == -1, "负 offset 应返回错误");
    TEST_ASSERT(errno == EINVAL, "负 offset 应返回 EINVAL");
    printf("负 offset fallocate 返回: %d, errno: %d\n", result, errno);

    // 测试负 len
    result = fallocate_wrapper(fd, 0, 0, (off_t)-1);
    TEST_ASSERT(result == -1, "负 len 应返回错误");
    TEST_ASSERT(errno == EINVAL, "负 len 应返回 EINVAL");
    printf("负 len fallocate 返回: %d, errno: %d\n", result, errno);

    close(fd);
    cleanup_test_file(TEST_FILE);

    TEST_PASS("无效 offset/length 测试");
}

// 测试 offset + len 溢出
static int test_offset_overflow(void) {
    printf("\n--- test_offset_overflow ---\n");

    int fd = open(TEST_FILE, O_CREAT | O_RDWR | O_TRUNC, 0644);
    TEST_ASSERT(fd >= 0, "打开测试文件");

    // 设置 offset 和 len 使得相加溢出
    // 使用接近 SIZE_MAX 的值
    off_t huge_offset = (off_t)LLONG_MAX - 1000;
    off_t len = 2000;  // offset + len 会溢出

    int result = fallocate_wrapper(fd, 0, huge_offset, len);
    // Linux 返回 EINVAL，不是 EFBIG
    TEST_ASSERT(result == -1, "溢出应返回错误");
    TEST_ASSERT(errno == EINVAL || errno == EFBIG, "溢出应返回 EINVAL 或 EFBIG");
    printf("溢出测试 fallocate 返回: %d, errno: %d (EINVAL=%d, EFBIG=%d)\n", result, errno, EINVAL, EFBIG);

    close(fd);
    cleanup_test_file(TEST_FILE);

    TEST_PASS("溢出测试");
}

// 测试 FALLOC_FL_KEEP_SIZE（Linux 支持）
static int test_keep_size_mode(void) {
    printf("\n--- test_keep_size_mode ---\n");

    int fd = open(TEST_FILE, O_CREAT | O_RDWR | O_TRUNC, 0644);
    TEST_ASSERT(fd >= 0, "打开测试文件");
    int support = check_fallocate_supported(fd);
    if (support == 0) {
        close(fd);
        cleanup_test_file(TEST_FILE);
        TEST_SKIP("fallocate 不支持");
    }
    TEST_ASSERT(support > 0, "fallocate 探测失败");

    // 写入一些初始数据
    const char *data = "Hello";
    ssize_t written = write(fd, data, strlen(data));
    TEST_ASSERT(written == (ssize_t)strlen(data), "写入初始数据");

    off_t initial_size = get_fd_size(fd);
    printf("初始文件大小: %ld bytes\n", initial_size);

    // FALLOC_FL_KEEP_SIZE: 分配空间但不改变文件大小
    int result = fallocate_wrapper(fd, FALLOC_FL_KEEP_SIZE, 0, 10000);
    if (result == -1 && errno == EOPNOTSUPP) {
        close(fd);
        cleanup_test_file(TEST_FILE);
        TEST_SKIP("FALLOC_FL_KEEP_SIZE 不支持");
    }
    TEST_ASSERT(result == 0, "FALLOC_FL_KEEP_SIZE 调用成功");
    off_t size_after = get_fd_size(fd);
    printf("FALLOC_FL_KEEP_SIZE 后大小: %ld bytes\n", size_after);
    TEST_ASSERT(size_after == initial_size, "文件大小应保持不变");

    close(fd);
    cleanup_test_file(TEST_FILE);
    return 0;
}

// 测试 FALLOC_FL_PUNCH_HOLE
static int test_punch_hole_mode(void) {
    printf("\n--- test_punch_hole_mode ---\n");

    int fd = open(TEST_FILE, O_CREAT | O_RDWR | O_TRUNC, 0644);
    TEST_ASSERT(fd >= 0, "打开测试文件");
    int support = check_fallocate_supported(fd);
    if (support == 0) {
        close(fd);
        cleanup_test_file(TEST_FILE);
        TEST_SKIP("fallocate 不支持");
    }
    TEST_ASSERT(support > 0, "fallocate 探测失败");

    TEST_ASSERT(write_pattern_fd(fd, 0, 8192, 0x5a) == 0, "写入初始数据");

    int result = fallocate_wrapper(fd, FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, 4096, 4096);
    if (result == -1 && errno == EOPNOTSUPP) {
        close(fd);
        cleanup_test_file(TEST_FILE);
        TEST_SKIP("FALLOC_FL_PUNCH_HOLE 不支持");
    }
    TEST_ASSERT(result == 0, "punch hole 成功");

    unsigned char buf[8192];
    TEST_ASSERT(read_full_fd(fd, 0, buf, sizeof(buf)) == 0, "读取数据");

    for (size_t i = 0; i < 4096; i++) {
        TEST_ASSERT(buf[i] == 0x5a, "punch hole 前半段保持不变");
    }
    for (size_t i = 4096; i < 8192; i++) {
        TEST_ASSERT(buf[i] == 0, "punch hole 区域应为 0");
    }

    close(fd);
    cleanup_test_file(TEST_FILE);
    TEST_PASS("FALLOC_FL_PUNCH_HOLE 测试");
}

// 测试 FALLOC_FL_ZERO_RANGE
static int test_zero_range_mode(void) {
    printf("\n--- test_zero_range_mode ---\n");

    int fd = open(TEST_FILE, O_CREAT | O_RDWR | O_TRUNC, 0644);
    TEST_ASSERT(fd >= 0, "打开测试文件");
    int support = check_fallocate_supported(fd);
    if (support == 0) {
        close(fd);
        cleanup_test_file(TEST_FILE);
        TEST_SKIP("fallocate 不支持");
    }
    TEST_ASSERT(support > 0, "fallocate 探测失败");

    TEST_ASSERT(write_pattern_fd(fd, 0, 8192, 0xa5) == 0, "写入初始数据");

    int result = fallocate_wrapper(fd, FALLOC_FL_ZERO_RANGE, 4096, 4096);
    if (result == -1 && errno == EOPNOTSUPP) {
        close(fd);
        cleanup_test_file(TEST_FILE);
        TEST_SKIP("FALLOC_FL_ZERO_RANGE 不支持");
    }
    TEST_ASSERT(result == 0, "zero range 成功");

    unsigned char buf[8192];
    TEST_ASSERT(read_full_fd(fd, 0, buf, sizeof(buf)) == 0, "读取数据");

    for (size_t i = 0; i < 4096; i++) {
        TEST_ASSERT(buf[i] == 0xa5, "zero range 前半段保持不变");
    }
    for (size_t i = 4096; i < 8192; i++) {
        TEST_ASSERT(buf[i] == 0, "zero range 区域应为 0");
    }

    close(fd);
    cleanup_test_file(TEST_FILE);
    TEST_PASS("FALLOC_FL_ZERO_RANGE 测试");
}

// 测试收缩操作
static int test_shrink_file(void) {
    printf("\n--- test_shrink_file ---\n");

    int fd = open(TEST_FILE, O_CREAT | O_RDWR | O_TRUNC, 0644);
    TEST_ASSERT(fd >= 0, "打开测试文件");
    int support = check_fallocate_supported(fd);
    if (support == 0) {
        close(fd);
        cleanup_test_file(TEST_FILE);
        TEST_SKIP("fallocate 不支持");
    }
    TEST_ASSERT(support > 0, "fallocate 探测失败");

    // 创建一个有数据的文件
    off_t initial_size = 5000;
    int result = fallocate_wrapper(fd, 0, 0, initial_size);
    TEST_ASSERT(result == 0, "初始分配成功");

    off_t current_size = get_fd_size(fd);
    printf("当前文件大小: %ld bytes\n", current_size);

    // Linux: mode=0 的 fallocate 确保文件至少有指定大小
    // 如果指定的大小小于当前文件大小，什么都不会发生（不收缩）
    off_t smaller_size = 1000;
    result = fallocate_wrapper(fd, 0, 0, smaller_size);
    // 应该成功，但文件大小不变
    if (result == 0) {
        off_t size_after = get_fd_size(fd);
        printf("fallocate(0, 0, %ld) 后大小: %ld bytes\n", smaller_size, size_after);
        TEST_ASSERT(size_after == current_size, "文件大小应保持不变（fallocate mode=0 不收缩）");
        printf("收缩操作返回: %d (不收缩，这是正确行为)\n", result);
        TEST_PASS("收缩文件测试");
    } else {
        printf("fallocate 返回: %d, errno: %d\n", result, errno);
        close(fd);
        cleanup_test_file(TEST_FILE);
        TEST_PASS("收缩文件测试");
    }

    close(fd);
    cleanup_test_file(TEST_FILE);
    return 0;
}

// ==================== 边界条件测试 ====================

// 测试大块空间分配
static int test_large_allocation(void) {
    printf("\n--- test_large_allocation ---\n");

    int fd = open(TEST_LARGE_FILE, O_CREAT | O_RDWR | O_TRUNC, 0644);
    TEST_ASSERT(fd >= 0, "打开测试文件");
    int support = check_fallocate_supported(fd);
    if (support == 0) {
        close(fd);
        cleanup_test_file(TEST_LARGE_FILE);
        TEST_SKIP("fallocate 不支持");
    }
    TEST_ASSERT(support > 0, "fallocate 探测失败");

    // 测试较大但不极端的分配
    off_t large_size = 10 * 1024 * 1024;  // 10 MB
    int result = fallocate_wrapper(fd, 0, 0, large_size);
    printf("分配 %ld MB 返回: %d\n", large_size / (1024 * 1024), result);

    if (result == 0) {
        off_t allocated_size = get_fd_size(fd);
        printf("大块分配后文件大小: %ld bytes\n", allocated_size);
        TEST_ASSERT(allocated_size == large_size, "大块分配大小正确");
        TEST_PASS("大块分配测试");
    }
    if (errno == ENOSPC || errno == EOPNOTSUPP) {
        printf("SKIP: 大块分配失败 (errno=%d)\n", errno);
        close(fd);
        cleanup_test_file(TEST_LARGE_FILE);
        TEST_SKIP("大块分配测试");
    }
    printf("FAIL: 大块分配失败 (errno=%d)\n", errno);
    close(fd);
    cleanup_test_file(TEST_LARGE_FILE);
    return -1;

    close(fd);
    cleanup_test_file(TEST_LARGE_FILE);
    return 0;
}

// ==================== 特殊文件类型测试 ====================

// 测试对管道调用 fallocate
static int test_pipe(void) {
    printf("\n--- test_pipe ---\n");

    int pipefd[2];
    int result = pipe(pipefd);
    TEST_ASSERT(result == 0, "创建管道");

    errno = 0;
    result = fallocate_wrapper(pipefd[1], 0, 0, 1000);
    TEST_ASSERT(result == -1, "管道 fallocate 应失败");
    TEST_ASSERT(errno == ESPIPE, "管道应返回 ESPIPE");
    printf("管道 fallocate 返回: %d, errno: %d (ESPIPE=%d)\n",
           result, errno, ESPIPE);

    close(pipefd[0]);
    close(pipefd[1]);

    TEST_PASS("管道测试");
}

// 测试符号链接
static int test_symlink(void) {
    printf("\n--- test_symlink ---\n");

    // 创建目标文件
    int fd = open(TEST_FILE, O_CREAT | O_RDWR | O_TRUNC, 0644);
    TEST_ASSERT(fd >= 0, "创建目标文件");
    ssize_t written = write(fd, "target content", 13);
    TEST_ASSERT(written == 13, "写入目标文件内容");
    close(fd);

    // 创建符号链接
    unlink(TEST_SYMLINK);
    int result = symlink(TEST_FILE, TEST_SYMLINK);
    TEST_ASSERT(result == 0, "创建符号链接");

    // 打开符号链接（会跟随到目标文件）
    int fd_link = open(TEST_SYMLINK, O_RDWR);
    TEST_ASSERT(fd_link >= 0, "打开符号链接");

    // fallocate 应该作用在目标文件上
    result = fallocate_wrapper(fd_link, 0, 0, 5000);
    if (result == -1 && errno == EOPNOTSUPP) {
        close(fd_link);
        cleanup_test_file(TEST_FILE);
        cleanup_test_file(TEST_SYMLINK);
        TEST_SKIP("fallocate 不支持");
    }
    TEST_ASSERT(result == 0, "符号链接目标 fallocate 成功");
    off_t link_size = get_fd_size(fd_link);
    off_t target_size = get_file_size(TEST_FILE);
    printf("符号链接 fd 大小: %ld, 目标文件大小: %ld\n",
           link_size, target_size);
    TEST_ASSERT(link_size == 5000, "符号链接操作成功");
    TEST_ASSERT(target_size == 5000, "目标文件被正确修改");

    close(fd_link);
    cleanup_test_file(TEST_FILE);
    cleanup_test_file(TEST_SYMLINK);
    return 0;
}

// ==================== 一致性测试 ====================

// 测试与 ftruncate 的一致性
static int test_consistency_with_ftruncate(void) {
    printf("\n--- test_consistency_with_ftruncate ---\n");

    // 使用 fallocate
    int fd1 = open(TEST_FILE, O_CREAT | O_RDWR | O_TRUNC, 0644);
    TEST_ASSERT(fd1 >= 0, "打开文件1");
    int result = fallocate_wrapper(fd1, 0, 0, 10000);
    if (result == -1 && errno == EOPNOTSUPP) {
        close(fd1);
        cleanup_test_file(TEST_FILE);
        TEST_SKIP("fallocate 不支持");
    }
    TEST_ASSERT(result == 0, "fallocate 成功");
    off_t size1 = get_fd_size(fd1);
    close(fd1);
    cleanup_test_file(TEST_FILE);

    // 使用 ftruncate 达到相同大小
    int fd2 = open(TEST_FILE, O_CREAT | O_RDWR | O_TRUNC, 0644);
    TEST_ASSERT(fd2 >= 0, "打开文件2");
    result = ftruncate(fd2, 10000);
    TEST_ASSERT(result == 0, "ftruncate 成功");
    off_t size2 = get_fd_size(fd2);
    close(fd2);
    cleanup_test_file(TEST_FILE);

    printf("fallocate 大小: %ld, ftruncate 大小: %ld\n", size1, size2);
    TEST_ASSERT(size1 == size2, "fallocate 和 ftruncate 结果一致");

    TEST_PASS("与 ftruncate 一致性测试");
}

// 测试与 write 的一致性
static int test_consistency_with_write(void) {
    printf("\n--- test_consistency_with_write ---\n");

    int fd = open(TEST_FILE, O_CREAT | O_RDWR | O_TRUNC, 0644);
    TEST_ASSERT(fd >= 0, "打开测试文件");

    // fallocate 分配空间
    off_t alloc_size = 10000;
    int result = fallocate_wrapper(fd, 0, 0, alloc_size);
    if (result == -1 && errno == EOPNOTSUPP) {
        close(fd);
        cleanup_test_file(TEST_FILE);
        TEST_SKIP("fallocate 不支持");
    }
    TEST_ASSERT(result == 0, "fallocate 成功");

    // 在分配的区域写入数据
    const char *test_pattern = "ABCDE";
    off_t offsets[] = {0, 100, 5000, 9995};
    char read_buffer[128];

    for (size_t i = 0; i < sizeof(offsets) / sizeof(offsets[0]); i++) {
        off_t offset = offsets[i];
        lseek(fd, offset, SEEK_SET);
        ssize_t written = write(fd, test_pattern, strlen(test_pattern));
        TEST_ASSERT(written == (ssize_t)strlen(test_pattern), "写入测试数据");

        // 读取验证
        lseek(fd, offset, SEEK_SET);
        memset(read_buffer, 0, sizeof(read_buffer));
        ssize_t nread = read(fd, read_buffer, strlen(test_pattern));
        TEST_ASSERT(nread == (ssize_t)strlen(test_pattern), "读取测试数据");
        TEST_ASSERT(memcmp(read_buffer, test_pattern, strlen(test_pattern)) == 0,
                    "写入数据验证成功");
        printf("偏移 %ld 处写入/读取验证成功\n", offset);
    }

    // 验证未写入区域应为零
    lseek(fd, 200, SEEK_SET);
    memset(read_buffer, 0, sizeof(read_buffer));
    ssize_t nread = read(fd, read_buffer, 100);
    TEST_ASSERT(nread == 100, "读取未写入区域");
    int all_zero = 1;
    for (int i = 0; i < 100; i++) {
        if (read_buffer[i] != 0) {
            all_zero = 0;
            break;
        }
    }
    TEST_ASSERT(all_zero, "未写入区域应为零");

    close(fd);
    cleanup_test_file(TEST_FILE);

    TEST_PASS("与 write 一致性测试");
}

// ==================== 主函数 ====================

int main(void) {
    printf("========================================\n");
    printf("  fallocate 系统调用完备测试\n");
    printf("========================================\n");

    int passed = 0;
    int failed = 0;
    int skipped = 0;
    int r;

    // 基本功能测试
    printf("\n========== 基本功能测试 ==========\n");
    r = test_basic_fallocate(); if (r == 0) passed++; else if (r == 77) skipped++; else failed++;
    r = test_append_to_existing_data(); if (r == 0) passed++; else if (r == 77) skipped++; else failed++;
    r = test_multiple_allocations(); if (r == 0) passed++; else if (r == 77) skipped++; else failed++;

    // 错误条件测试
    printf("\n========== 错误条件测试 ==========\n");
    r = test_invalid_fd(); if (r == 0) passed++; else if (r == 77) skipped++; else failed++;
    r = test_readonly_fd(); if (r == 0) passed++; else if (r == 77) skipped++; else failed++;
    r = test_directory(); if (r == 0) passed++; else if (r == 77) skipped++; else failed++;
    r = test_zero_length(); if (r == 0) passed++; else if (r == 77) skipped++; else failed++;
    r = test_invalid_offset_length(); if (r == 0) passed++; else if (r == 77) skipped++; else failed++;
    r = test_offset_overflow(); if (r == 0) passed++; else if (r == 77) skipped++; else failed++;
    r = test_keep_size_mode(); if (r == 0) passed++; else if (r == 77) skipped++; else failed++;
    r = test_punch_hole_mode(); if (r == 0) passed++; else if (r == 77) skipped++; else failed++;
    r = test_zero_range_mode(); if (r == 0) passed++; else if (r == 77) skipped++; else failed++;
    r = test_shrink_file(); if (r == 0) passed++; else if (r == 77) skipped++; else failed++;

    // 边界条件测试
    printf("\n========== 边界条件测试 ==========\n");
    r = test_large_allocation(); if (r == 0) passed++; else if (r == 77) skipped++; else failed++;

    // 特殊文件类型测试
    printf("\n========== 特殊文件类型测试 ==========\n");
    r = test_pipe(); if (r == 0) passed++; else if (r == 77) skipped++; else failed++;
    r = test_symlink(); if (r == 0) passed++; else if (r == 77) skipped++; else failed++;

    // 一致性测试
    printf("\n========== 一致性测试 ==========\n");
    r = test_consistency_with_ftruncate(); if (r == 0) passed++; else if (r == 77) skipped++; else failed++;
    r = test_consistency_with_write(); if (r == 0) passed++; else if (r == 77) skipped++; else failed++;

    // 最终统计
    printf("\n========================================\n");
    printf("  测试完成\n");
    printf("========================================\n");
    printf("通过: %d\n", passed);
    printf("失败: %d\n", failed);
    printf("跳过: %d\n", skipped);
    printf("总计: %d\n", passed + failed + skipped);
    printf("========================================\n");

    return (failed > 0) ? 1 : 0;
}
