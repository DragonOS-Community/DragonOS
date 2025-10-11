#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/uio.h>
#include <unistd.h>

#define TEST_FILE "test_pwritev.txt"
#define BUFFER_SIZE 256

/**
 * @brief 测试 pwritev 系统调用的基本功能
 * 
 * 该测试程序验证 pwritev 系统调用的以下功能:
 * 1. 基本的散布写入功能
 * 2. 在指定偏移量处写入数据
 * 3. 不改变文件当前偏移量
 * 4. 多个 iovec 的处理
 * 5. 错误处理（无效的文件描述符）
 */

/**
 * @brief 测试基本的 pwritev 功能
 * 
 * @return int 成功返回 0，失败返回 -1
 */
int test_basic_pwritev() {
    printf("\n=== Test 1: Basic pwritev functionality ===\n");
    
    int fd = open(TEST_FILE, O_RDWR | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) {
        perror("open");
        return -1;
    }
    
    // 首先写入一些初始数据
    const char *init_data = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    ssize_t written = write(fd, init_data, strlen(init_data));
    if (written < 0) {
        perror("write initial data");
        close(fd);
        return -1;
    }
    printf("Initial data written: %s (%ld bytes)\n", init_data, written);
    
    // 准备要使用 pwritev 写入的数据
    char buf1[] = "Hello";
    char buf2[] = "World";
    char buf3[] = "!";
    
    struct iovec iov[3];
    iov[0].iov_base = buf1;
    iov[0].iov_len = strlen(buf1);
    iov[1].iov_base = buf2;
    iov[1].iov_len = strlen(buf2);
    iov[2].iov_base = buf3;
    iov[2].iov_len = strlen(buf3);
    
    // 在偏移量 10 处写入数据
    off_t offset = 10;
    ssize_t pwritten = pwritev(fd, iov, 3, offset);
    if (pwritten < 0) {
        perror("pwritev");
        close(fd);
        return -1;
    }
    printf("pwritev wrote %ld bytes at offset %ld\n", pwritten, offset);
    
    // 验证文件当前偏移量没有改变
    off_t current_offset = lseek(fd, 0, SEEK_CUR);
    if (current_offset != written) {
        printf("ERROR: File offset changed! Expected %ld, got %ld\n", written, current_offset);
        close(fd);
        return -1;
    }
    printf("File offset unchanged: %ld (correct)\n", current_offset);
    
    // 读取并验证写入的数据
    lseek(fd, 0, SEEK_SET);
    char read_buf[BUFFER_SIZE] = {0};
    ssize_t nread = read(fd, read_buf, BUFFER_SIZE);
    if (nread < 0) {
        perror("read");
        close(fd);
        return -1;
    }
    
    printf("File content after pwritev: %s\n", read_buf);
    
    // 验证数据是否正确写入
    char expected[BUFFER_SIZE];
    strcpy(expected, init_data);
    memcpy(expected + offset, "HelloWorld!", 11);
    
    if (strncmp(read_buf, expected, strlen(expected)) == 0) {
        printf("✓ Test 1 PASSED: Data written correctly at offset %ld\n", offset);
    } else {
        printf("✗ Test 1 FAILED: Expected '%s', got '%s'\n", expected, read_buf);
        close(fd);
        return -1;
    }
    
    close(fd);
    return 0;
}

/**
 * @brief 测试在文件末尾之后写入
 * 
 * @return int 成功返回 0，失败返回 -1
 */
int test_pwritev_beyond_eof() {
    printf("\n=== Test 2: Write beyond EOF ===\n");
    
    int fd = open(TEST_FILE, O_RDWR | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) {
        perror("open");
        return -1;
    }
    
    // 写入少量初始数据
    const char *init_data = "START";
    write(fd, init_data, strlen(init_data));
    
    // 在远超文件末尾的位置写入
    char buf1[] = "FAR";
    char buf2[] = "AWAY";
    
    struct iovec iov[2];
    iov[0].iov_base = buf1;
    iov[0].iov_len = strlen(buf1);
    iov[1].iov_base = buf2;
    iov[1].iov_len = strlen(buf2);
    
    off_t offset = 100;
    ssize_t pwritten = pwritev(fd, iov, 2, offset);
    if (pwritten < 0) {
        perror("pwritev");
        close(fd);
        return -1;
    }
    printf("pwritev wrote %ld bytes at offset %ld\n", pwritten, offset);
    
    // 读取并检查文件内容
    lseek(fd, 0, SEEK_SET);
    char read_buf[BUFFER_SIZE] = {0};
    ssize_t nread = read(fd, read_buf, BUFFER_SIZE);
    
    printf("File size after write: %ld bytes\n", nread);
    printf("Content at start: %s\n", read_buf);
    printf("Content at offset %ld: %s\n", offset, read_buf + offset);
    
    // 验证在偏移量处的数据
    if (strncmp(read_buf + offset, "FARAWAY", 7) == 0) {
        printf("✓ Test 2 PASSED: Data written beyond EOF correctly\n");
    } else {
        printf("✗ Test 2 FAILED: Data not written correctly\n");
        close(fd);
        return -1;
    }
    
    close(fd);
    return 0;
}

/**
 * @brief 测试使用无效文件描述符
 * 
 * @return int 成功返回 0，失败返回 -1
 */
int test_pwritev_invalid_fd() {
    printf("\n=== Test 3: Invalid file descriptor ===\n");
    
    char buf[] = "test";
    struct iovec iov[1];
    iov[0].iov_base = buf;
    iov[0].iov_len = strlen(buf);
    
    // 使用无效的文件描述符
    int invalid_fd = 9999;
    ssize_t result = pwritev(invalid_fd, iov, 1, 0);
    
    if (result < 0) {
        printf("✓ Test 3 PASSED: pwritev correctly returned error for invalid fd\n");
        return 0;
    } else {
        printf("✗ Test 3 FAILED: pwritev should have failed with invalid fd\n");
        return -1;
    }
}

/**
 * @brief 测试零长度写入
 * 
 * @return int 成功返回 0，失败返回 -1
 */
int test_pwritev_zero_length() {
    printf("\n=== Test 4: Zero length write ===\n");
    
    int fd = open(TEST_FILE, O_RDWR | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) {
        perror("open");
        return -1;
    }
    
    char buf[] = "test";
    struct iovec iov[1];
    iov[0].iov_base = buf;
    iov[0].iov_len = 0;  // 零长度
    
    ssize_t result = pwritev(fd, iov, 1, 0);
    
    if (result == 0) {
        printf("✓ Test 4 PASSED: Zero length write returned 0\n");
    } else {
        printf("✗ Test 4 FAILED: Zero length write returned %ld\n", result);
        close(fd);
        return -1;
    }
    
    close(fd);
    return 0;
}

/**
 * @brief 测试多个 iovec 结构
 * 
 * @return int 成功返回 0，失败返回 -1
 */
int test_pwritev_multiple_iovecs() {
    printf("\n=== Test 5: Multiple iovec structures ===\n");
    
    int fd = open(TEST_FILE, O_RDWR | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) {
        perror("open");
        return -1;
    }
    
    // 准备多个缓冲区
    char buf1[] = "First";
    char buf2[] = "-Second";
    char buf3[] = "-Third";
    char buf4[] = "-Fourth";
    char buf5[] = "-Fifth";
    
    struct iovec iov[5];
    iov[0].iov_base = buf1;
    iov[0].iov_len = strlen(buf1);
    iov[1].iov_base = buf2;
    iov[1].iov_len = strlen(buf2);
    iov[2].iov_base = buf3;
    iov[2].iov_len = strlen(buf3);
    iov[3].iov_base = buf4;
    iov[3].iov_len = strlen(buf4);
    iov[4].iov_base = buf5;
    iov[4].iov_len = strlen(buf5);
    
    ssize_t pwritten = pwritev(fd, iov, 5, 0);
    if (pwritten < 0) {
        perror("pwritev");
        close(fd);
        return -1;
    }
    
    printf("Wrote %ld bytes using 5 iovec structures\n", pwritten);
    
    // 读取并验证
    lseek(fd, 0, SEEK_SET);
    char read_buf[BUFFER_SIZE] = {0};
    ssize_t nread = read(fd, read_buf, BUFFER_SIZE);
    
    const char *expected = "First-Second-Third-Fourth-Fifth";
    printf("Expected: %s\n", expected);
    printf("Got:      %s\n", read_buf);
    
    if (strcmp(read_buf, expected) == 0) {
        printf("✓ Test 5 PASSED: Multiple iovecs handled correctly\n");
    } else {
        printf("✗ Test 5 FAILED: Content mismatch\n");
        close(fd);
        return -1;
    }
    
    close(fd);
    return 0;
}

/**
 * @brief 测试 pwritev 不改变文件偏移量
 * 
 * @return int 成功返回 0，失败返回 -1
 */
int test_pwritev_offset_preservation() {
    printf("\n=== Test 6: File offset preservation ===\n");
    
    int fd = open(TEST_FILE, O_RDWR | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) {
        perror("open");
        return -1;
    }
    
    // 写入初始数据
    const char *init_data = "ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    write(fd, init_data, strlen(init_data));
    
    // 设置文件偏移量到特定位置
    off_t seek_pos = 15;
    lseek(fd, seek_pos, SEEK_SET);
    
    // 使用 pwritev 在不同位置写入
    char buf[] = "123";
    struct iovec iov[1];
    iov[0].iov_base = buf;
    iov[0].iov_len = strlen(buf);
    
    pwritev(fd, iov, 1, 5);  // 在偏移量 5 处写入
    
    // 检查文件偏移量是否保持不变
    off_t current_pos = lseek(fd, 0, SEEK_CUR);
    
    if (current_pos == seek_pos) {
        printf("✓ Test 6 PASSED: File offset preserved at %ld\n", current_pos);
    } else {
        printf("✗ Test 6 FAILED: File offset changed from %ld to %ld\n", 
               seek_pos, current_pos);
        close(fd);
        return -1;
    }
    
    close(fd);
    return 0;
}

int main() {
    printf("========================================\n");
    printf("    pwritev System Call Test Suite    \n");
    printf("========================================\n");
    
    int failed = 0;
    
    if (test_basic_pwritev() != 0) failed++;
    if (test_pwritev_beyond_eof() != 0) failed++;
    if (test_pwritev_invalid_fd() != 0) failed++;
    if (test_pwritev_zero_length() != 0) failed++;
    if (test_pwritev_multiple_iovecs() != 0) failed++;
    if (test_pwritev_offset_preservation() != 0) failed++;
    
    printf("\n========================================\n");
    if (failed == 0) {
        printf("✓ All tests PASSED!\n");
    } else {
        printf("✗ %d test(s) FAILED!\n", failed);
    }
    printf("========================================\n");
    
    // 清理测试文件
    unlink(TEST_FILE);
    
    return failed;
}
