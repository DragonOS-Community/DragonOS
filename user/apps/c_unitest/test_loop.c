#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/ioctl.h>
#include <sys/stat.h> // 用于 fstat
#include <stdint.h>
#include <errno.h>
// 控制命令常量
#define LOOP_CTL_ADD        0x4C80
#define LOOP_CTL_REMOVE     0x4C81
#define LOOP_CTL_GET_FREE   0x4C82
#define LOOP_SET_FD         0x4C00
#define LOOP_CLR_FD         0x4C01
#define LOOP_SET_STATUS64   0x4C04
#define LOOP_GET_STATUS64   0x4C05
// 与内核通信的设备路径
#define LOOP_DEVICE_CONTROL "/dev/loop-control"
#define LO_FLAGS_READ_ONLY  0x1
#define TEST_FILE_NAME "test_image.img"
#define TEST_FILE_SIZE (1024 * 1024) // 测试镜像大小 1MB
struct loop_status64 {
    uint64_t lo_offset;
    uint64_t lo_sizelimit;
    uint32_t lo_flags;
    uint32_t __pad;
};

// 创建测试镜像文件
void create_test_file() {
    printf("Creating test file: %s with size %d bytes\n", TEST_FILE_NAME, TEST_FILE_SIZE);
    int fd = open(TEST_FILE_NAME, O_CREAT | O_TRUNC | O_RDWR, 0644);
    if (fd < 0) {
        perror("Failed to create test file");
        exit(EXIT_FAILURE);
    }
    // 写入零填充数据以确保文件占满容量
    char zero_block[512] = {0};
    for (int i = 0; i < TEST_FILE_SIZE / 512; ++i) {
        if (write(fd, zero_block, 512) != 512) {
            perror("Failed to write to test file");
            close(fd);
            exit(EXIT_FAILURE);
        }
    }
    printf("Test file created successfully.\n");
    close(fd);
}

int main() {
    int control_fd;
    int loop_minor;
    char loop_dev_path[64];
    int loop_fd;
    int backing_fd = -1;
    struct loop_status64 status;
    memset(&status, 0, sizeof(status));
    char write_buf[512] = "Hello Loop Device!";
    char read_buf[512];
    char verify_buf[512];

    create_test_file(); // 创建作为 loop 设备后端的文件

    backing_fd = open(TEST_FILE_NAME, O_RDWR);
    if (backing_fd < 0) {
        perror("Failed to open backing file");
        exit(EXIT_FAILURE);
    }

    // 1. 打开 loop-control 字符设备
    printf("Opening loop control device: %s\n", LOOP_DEVICE_CONTROL);
    control_fd = open(LOOP_DEVICE_CONTROL, O_RDWR);
    if (control_fd < 0) {
        perror("Failed to open loop control device. Make sure your kernel module is loaded and /dev/loop-control exists.");
        close(backing_fd);
        exit(EXIT_FAILURE);
    }
    printf("Loop control device opened successfully (fd=%d).\n", control_fd);

    // 2. 获取一个空闲的 loop 次设备号
    printf("Requesting a free loop device minor...\n");
    if (ioctl(control_fd, LOOP_CTL_GET_FREE, &loop_minor) < 0) {
        perror("Failed to get free loop device minor");
        close(backing_fd);
        close(control_fd);
        exit(EXIT_FAILURE);
    }
    printf("Got free loop minor: %d\n", loop_minor);

    // 3. 请求内核以该次设备号创建新的 loop 设备
    printf("Adding loop device loop%d...\n", loop_minor);
    int returned_minor = ioctl(control_fd, LOOP_CTL_ADD, loop_minor);
    if (returned_minor < 0) {
        perror("Failed to add loop device");
        printf("returned_minor: %d\n", returned_minor);
        close(backing_fd);
        close(control_fd);
        exit(EXIT_FAILURE);
    }
    if (returned_minor != loop_minor) {  
        fprintf(stderr, "Warning: LOOP_CTL_ADD returned minor %d, expected %d\n", returned_minor, loop_minor);
    }
    printf("Loop device loop%d added (kernel returned minor: %d).\n", loop_minor, returned_minor);

    // 4. 打开刚创建的块设备节点
    sprintf(loop_dev_path, "/dev/loop%d", loop_minor);
    printf("Attempting to open block device: %s\n", loop_dev_path);
    loop_fd = open(loop_dev_path, O_RDWR);
    if (loop_fd < 0) {
        perror("Failed to open loop block device. This might mean the block device node wasn't created/registered correctly, or permissions.");
        fprintf(stderr, "Make sure /dev/loop%d exists as a block device.\n", loop_minor);
        close(backing_fd);
        close(control_fd);
        exit(EXIT_FAILURE);
    }
    printf("Loop block device %s opened successfully (fd=%d).\n", loop_dev_path, loop_fd);

    printf("Associating backing file %s with loop device using LOOP_SET_FD...\n", TEST_FILE_NAME);
    if (ioctl(loop_fd, LOOP_SET_FD, backing_fd) < 0) {
        perror("Failed to associate backing file with loop device");
        close(loop_fd);
        close(backing_fd);
        close(control_fd);
        exit(EXIT_FAILURE);
    }
    printf("Backing file associated successfully.\n");

    // 配置偏移和大小限制，使 loop 设备从文件第 512 字节开始映射
    status.lo_offset = 512;
    status.lo_sizelimit = TEST_FILE_SIZE - status.lo_offset;
    status.lo_flags = 0;
    status.__pad = 0;

    printf("配置 loop 设备的偏移和大小限制...\n");
    if (ioctl(loop_fd, LOOP_SET_STATUS64, &status) < 0) {
        perror("Failed to set loop status64");
        close(loop_fd);
        close(backing_fd);
        close(control_fd);
        exit(EXIT_FAILURE);
    }

    struct loop_status64 status_readback = {0};
    if (ioctl(loop_fd, LOOP_GET_STATUS64, &status_readback) < 0) {
        perror("Failed to get loop status64");
        close(loop_fd);
        close(backing_fd);
        close(control_fd);
        exit(EXIT_FAILURE);
    }
    printf("loop 偏移: %llu, 映射字节数: %llu, 标志: 0x%x\n",
           (unsigned long long)status_readback.lo_offset,
           (unsigned long long)status_readback.lo_sizelimit,
           status_readback.lo_flags);

    if (status_readback.lo_offset != status.lo_offset ||
        status_readback.lo_sizelimit != status.lo_sizelimit) {
        fprintf(stderr, "Loop status mismatch after configuration.\n");
        close(loop_fd);
        close(backing_fd);
        close(control_fd);
        exit(EXIT_FAILURE);
    }

    status = status_readback;

    // 5. 对 loop 块设备执行读写测试

    printf("Writing to loop device %s...\n", loop_dev_path);
    if (lseek(loop_fd, 0, SEEK_SET) < 0) {
        perror("lseek failed before write");
        close(loop_fd);
        close(backing_fd);
        close(control_fd);
        exit(EXIT_FAILURE);
    }
    if (write(loop_fd, write_buf, sizeof(write_buf)) != sizeof(write_buf)) {
        perror("Failed to write to loop device");
        close(loop_fd);
        close(backing_fd);
        close(control_fd);
        exit(EXIT_FAILURE);
    }
    printf("Successfully wrote '%s' to loop device.\n", write_buf);

    // 校验后端文件对应偏移512字节的数据是否与写入内容一致
    int verify_fd = open(TEST_FILE_NAME, O_RDONLY);
    if (verify_fd < 0) {
        perror("Failed to reopen backing file for verification");
        close(loop_fd);
        close(backing_fd);
        close(control_fd);
        exit(EXIT_FAILURE);
    }
    if (lseek(verify_fd, (off_t)status.lo_offset, SEEK_SET) < 0) {
        perror("Failed to seek backing file");
        close(verify_fd);
        close(loop_fd);
        close(backing_fd);
        close(control_fd);
        exit(EXIT_FAILURE);
    }
    if (read(verify_fd, verify_buf, sizeof(write_buf)) != sizeof(write_buf)) {
        perror("Failed to read back from backing file");
        close(verify_fd);
        close(loop_fd);
        close(backing_fd);
        close(control_fd);
        exit(EXIT_FAILURE);
    }
    close(verify_fd);
    if (memcmp(write_buf, verify_buf, sizeof(write_buf)) != 0) {
        fprintf(stderr, "Backing file data mismatch.\n");
        close(loop_fd);
        close(backing_fd);
        close(control_fd);
        exit(EXIT_FAILURE);
    }
    printf("镜像文件内容验证通过。\n");

    printf("Reading from loop device %s...\n", loop_dev_path);
    memset(read_buf, 0, sizeof(read_buf));
    if (lseek(loop_fd, 0, SEEK_SET) < 0) {
        perror("lseek failed before read");
        close(loop_fd);
        close(backing_fd);
        close(control_fd);
        exit(EXIT_FAILURE);
    }
    if (read(loop_fd, read_buf, sizeof(read_buf)) != sizeof(read_buf)) {
        perror("Failed to read from loop device");
        close(loop_fd);
        close(backing_fd);
        close(control_fd);
        exit(EXIT_FAILURE);
    }
    printf("Successfully read '%s' from loop device.\n", read_buf);

    if (strcmp(write_buf, read_buf) == 0) {
        printf("Read/write test PASSED.\n");
    } else {
        printf("Read/write test FAILED: Mismatch in data.\n");
    }

    // 将设备切换到只读模式，验证写入被阻止
    printf("切换 loop 设备为只读模式...\n");
    status.lo_flags |= LO_FLAGS_READ_ONLY;
    if (ioctl(loop_fd, LOOP_SET_STATUS64, &status) < 0) {
        perror("Failed to enable read-only flag");
        close(loop_fd);
        close(backing_fd);
        close(control_fd);
        exit(EXIT_FAILURE);
    }

    errno = 0;
    if (lseek(loop_fd, 0, SEEK_SET) < 0) {
        perror("Failed to seek loop device");
    }
    if (write(loop_fd, write_buf, sizeof(write_buf)) >= 0 || errno != EROFS) {
        fprintf(stderr, "Write unexpectedly succeeded under read-only mode (errno=%d).\n", errno);
        close(loop_fd);
        close(backing_fd);
        close(control_fd);
        exit(EXIT_FAILURE);
    }
    printf("只读模式下写入被正确阻止。\n");

    status.lo_flags &= ~LO_FLAGS_READ_ONLY;
    if (ioctl(loop_fd, LOOP_SET_STATUS64, &status) < 0) {
        perror("Failed to restore writeable mode");
        close(loop_fd);
        close(backing_fd);
        close(control_fd);
        exit(EXIT_FAILURE);
    }

    // 6. 清理并删除 loop 设备
    printf("Clearing loop device loop%d backing file...\n", loop_minor);
    if (ioctl(loop_fd, LOOP_CLR_FD, 0) < 0) {
        perror("Failed to clear loop device backing file");
    }

    printf("Removing loop device loop%d...\n", loop_minor);
    if (ioctl(control_fd, LOOP_CTL_REMOVE, loop_minor) < 0) {
        perror("Failed to remove loop device");
        close(loop_fd);
        close(backing_fd);
        close(control_fd);
        exit(EXIT_FAILURE);
    }
    printf("Loop device loop%d removed successfully.\n", loop_minor);

    // 释放资源并删除测试文件
    close(loop_fd);
    close(backing_fd);
    close(control_fd);
    unlink(TEST_FILE_NAME);
    printf("All tests completed. Cleaned up.\n");

    // 校验设备删除后不可再次打开
    int reopen_fd = open(loop_dev_path, O_RDWR);
    if (reopen_fd >= 0) {
        printf("Unexpectedly reopened %s after removal (fd=%d).\n", loop_dev_path, reopen_fd);
        close(reopen_fd);
        return EXIT_FAILURE;
    } else {
        printf("Confirmed %s is inaccessible after removal (errno=%d).\n", loop_dev_path, errno);
    }

    return 0;
}
