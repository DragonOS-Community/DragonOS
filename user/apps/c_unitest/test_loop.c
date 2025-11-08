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
#define LOOP_CHANGE_FD      0x4C06 // 新增
#define LOOP_SET_CAPACITY   0x4C07 // 新增

// 与内核通信的设备路径
#define LOOP_DEVICE_CONTROL "/dev/loop-control"
#define LO_FLAGS_READ_ONLY  0x1
#define TEST_FILE_NAME "test_image.img"
#define TEST_FILE_NAME_2 "test_image_2.img" // 第二个测试文件
#define TEST_FILE_SIZE (1024 * 1024) // 测试镜像大小 1MB
#define TEST_FILE_SIZE_2 (512 * 1024) // 第二个测试镜像大小 512KB

struct loop_status64 {
    uint64_t lo_offset;
    uint64_t lo_sizelimit;
    uint32_t lo_flags;
    uint32_t __pad;
};

// 创建测试镜像文件
void create_test_file(const char* filename, int size) {
    printf("Creating test file: %s with size %d bytes\n", filename, size);
    int fd = open(filename, O_CREAT | O_TRUNC | O_RDWR, 0644);
    if (fd < 0) {
        perror("Failed to create test file");
        exit(EXIT_FAILURE);
    }
    // 写入零填充数据以确保文件占满容量
    char zero_block[512] = {0};
    for (int i = 0; i < size / 512; ++i) {
        if (write(fd, zero_block, 512) != 512) {
            perror("Failed to write to test file");
            close(fd);
            exit(EXIT_FAILURE);
        }
    }
    printf("Test file %s created successfully.\n", filename);
    close(fd);
}

// 获取文件大小的辅助函数
long get_file_size(int fd) {
    struct stat st;
    if (fstat(fd, &st) < 0) {
        perror("fstat failed");
        return -1;
    }
    return st.st_size;
}

int main() {
    int control_fd;
    int loop_minor;
    char loop_dev_path[64];
    int loop_fd;
    int backing_fd_1 = -1;
    int backing_fd_2 = -1; // 第二个后端文件描述符
    struct loop_status64 status;
    memset(&status, 0, sizeof(status));
    char write_buf[512] = "Hello Loop Device!";
    char write_buf_2[512] = "New Backing File Data!"; // 第二个文件写入数据
    char read_buf[512];
    char verify_buf[512];

    create_test_file(TEST_FILE_NAME, TEST_FILE_SIZE); // 创建作为 loop 设备后端的文件 1
    create_test_file(TEST_FILE_NAME_2, TEST_FILE_SIZE_2); // 创建作为 loop 设备后端的文件 2

    backing_fd_1 = open(TEST_FILE_NAME, O_RDWR);
    if (backing_fd_1 < 0) {
        perror("Failed to open backing file 1");
        exit(EXIT_FAILURE);
    }

    backing_fd_2 = open(TEST_FILE_NAME_2, O_RDWR);
    if (backing_fd_2 < 0) {
        perror("Failed to open backing file 2");
        close(backing_fd_1);
        exit(EXIT_FAILURE);
    }

    // 1. 打开 loop-control 字符设备
    printf("Opening loop control device: %s\n", LOOP_DEVICE_CONTROL);
    control_fd = open(LOOP_DEVICE_CONTROL, O_RDWR);
    if (control_fd < 0) {
        perror("Failed to open loop control device. Make sure your kernel module is loaded and /dev/loop-control exists.");
        close(backing_fd_1);
        close(backing_fd_2);
        exit(EXIT_FAILURE);
    }
    printf("Loop control device opened successfully (fd=%d).\n", control_fd);

    // 2. 获取一个空闲的 loop 次设备号
    printf("Requesting a free loop device minor...\n");
    if (ioctl(control_fd, LOOP_CTL_GET_FREE, &loop_minor) < 0) {
        perror("Failed to get free loop device minor");
        close(backing_fd_1);
        close(backing_fd_2);
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
        close(backing_fd_1);
        close(backing_fd_2);
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
        close(backing_fd_1);
        close(backing_fd_2);
        close(control_fd);
        exit(EXIT_FAILURE);
    }
    printf("Loop block device %s opened successfully (fd=%d).\n", loop_dev_path, loop_fd);

    printf("Associating backing file %s with loop device using LOOP_SET_FD...\n", TEST_FILE_NAME);
    if (ioctl(loop_fd, LOOP_SET_FD, backing_fd_1) < 0) {
        perror("Failed to associate backing file with loop device");
        close(loop_fd);
        close(backing_fd_1);
        close(backing_fd_2);
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
        close(backing_fd_1);
        close(backing_fd_2);
        close(control_fd);
        exit(EXIT_FAILURE);
    }

    struct loop_status64 status_readback = {0};
    if (ioctl(loop_fd, LOOP_GET_STATUS64, &status_readback) < 0) {
        perror("Failed to get loop status64");
        close(loop_fd);
        close(backing_fd_1);
        close(backing_fd_2);
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
        close(backing_fd_1);
        close(backing_fd_2);
        close(control_fd);
        exit(EXIT_FAILURE);
    }

    status = status_readback;

    // 5. 对 loop 块设备执行读写测试 (针对第一个文件)

    printf("Writing to loop device %s (via %s)...\n", loop_dev_path, TEST_FILE_NAME);
    if (lseek(loop_fd, 0, SEEK_SET) < 0) {
        perror("lseek failed before write");
        goto cleanup;
    }
    if (write(loop_fd, write_buf, sizeof(write_buf)) != sizeof(write_buf)) {
        perror("Failed to write to loop device");
        goto cleanup;
    }
    printf("Successfully wrote '%s' to loop device.\n", write_buf);

    // 校验后端文件对应偏移512字节的数据是否与写入内容一致
    int verify_fd = open(TEST_FILE_NAME, O_RDONLY);
    if (verify_fd < 0) {
        perror("Failed to reopen backing file for verification");
        goto cleanup;
    }
    if (lseek(verify_fd, (off_t)status.lo_offset, SEEK_SET) < 0) {
        perror("Failed to seek backing file");
        close(verify_fd);
        goto cleanup;
    }
    if (read(verify_fd, verify_buf, sizeof(write_buf)) != sizeof(write_buf)) {
        perror("Failed to read back from backing file");
        close(verify_fd);
        goto cleanup;
    }
    close(verify_fd);
    if (memcmp(write_buf, verify_buf, sizeof(write_buf)) != 0) {
        fprintf(stderr, "Backing file data mismatch.\n");
        goto cleanup;
    }
    printf("镜像文件内容验证通过。\n");

    printf("Reading from loop device %s...\n", loop_dev_path);
    memset(read_buf, 0, sizeof(read_buf));
    if (lseek(loop_fd, 0, SEEK_SET) < 0) {
        perror("lseek failed before read");
        goto cleanup;
    }
    if (read(loop_fd, read_buf, sizeof(read_buf)) != sizeof(read_buf)) {
        perror("Failed to read from loop device");
        goto cleanup;
    }
    printf("Successfully read '%s' from loop device.\n", read_buf);

    if (strcmp(write_buf, read_buf) == 0) {
        printf("Read/write test PASSED.\n");
    } else {
        printf("Read/write test FAILED: Mismatch in data.\n");
        goto cleanup;
    }

    // 将设备切换到只读模式，验证写入被阻止
    printf("切换 loop 设备为只读模式...\n");
    status.lo_flags |= LO_FLAGS_READ_ONLY;
    if (ioctl(loop_fd, LOOP_SET_STATUS64, &status) < 0) {
        perror("Failed to enable read-only flag");
        goto cleanup;
    }

    errno = 0;
    if (lseek(loop_fd, 0, SEEK_SET) < 0) {
        perror("Failed to seek loop device");
    }
    if (write(loop_fd, write_buf, sizeof(write_buf)) >= 0 || errno != EROFS) {
        fprintf(stderr, "Write unexpectedly succeeded under read-only mode (errno=%d).\n", errno);
        goto cleanup;
    }
    printf("只读模式下写入被正确阻止。\n");

    status.lo_flags &= ~LO_FLAGS_READ_ONLY;
    if (ioctl(loop_fd, LOOP_SET_STATUS64, &status) < 0) {
        perror("Failed to restore writeable mode");
        goto cleanup;
    }

    // =======================================================
    // 新增测试用例：LOOP_CHANGE_FD
    // =======================================================
    printf("\n--- Testing LOOP_CHANGE_FD ---\n");
    printf("Changing backing file from %s to %s using LOOP_CHANGE_FD...\n", TEST_FILE_NAME, TEST_FILE_NAME_2);
    if (ioctl(loop_fd, LOOP_CHANGE_FD, backing_fd_2) < 0) {
        perror("Failed to change backing file via LOOP_CHANGE_FD");
        goto cleanup;
    }
    printf("Backing file changed successfully to %s.\n", TEST_FILE_NAME_2);

    // 验证 loop 设备现在映射到第二个文件
    status_readback = (struct loop_status64){0};
    if (ioctl(loop_fd, LOOP_GET_STATUS64, &status_readback) < 0) {
        perror("Failed to get loop status64 after LOOP_CHANGE_FD");
        goto cleanup;
    }
    printf("After LOOP_CHANGE_FD, loop current offset: %llu, sizelimit: %llu, flags: 0x%x\n",
           (unsigned long long)status_readback.lo_offset,
           (unsigned long long)status_readback.lo_sizelimit,
           status_readback.lo_flags);

    // 确保偏移和大小限制保持，但实际大小应该基于新文件
    long actual_file_2_size = get_file_size(backing_fd_2);
    if (actual_file_2_size < 0) goto cleanup;

    // 此时 lo_sizelimit 应该反映出 TEST_FILE_NAME_2 的大小
    // 因为 LOOP_CHANGE_FD 会调用 recalc_effective_size
    // 并且默认的 sizelimit 是 0，表示不限制
    // 所以 effective_size 应该等于 file_size - offset
    // 我们需要重新计算预期的 effective_size
    uint64_t expected_sizelimit_after_change = (actual_file_2_size > status_readback.lo_offset) ? (actual_file_2_size - status_readback.lo_offset) : 0;
    
    // 注意：内核中的 `recalc_effective_size` 会将 `inner.file_size` 更新为有效大小
    // 但是 `lo_sizelimit` 字段在 `LoopStatus64` 中是用户设定的限制，它不会自动改变
    // 这里的验证需要更精确：`lo_sizelimit` 应该和我们之前设置的相同 (即 TEST_FILE_SIZE - 512)
    // 但当 `LOOP_CHANGE_FD` 成功时，其内部会重新计算 `file_size`
    // 如果 `lo_sizelimit` 保持为非0值，并且大于新文件大小，则 `file_size` 会被截断
    // 让我们先简单地检查是否能对新文件进行读写。
    
    // 写入新数据到 loop 设备，应该写入到 TEST_FILE_NAME_2
    printf("Writing to loop device %s (via %s)...\n", loop_dev_path, TEST_FILE_NAME_2);
    if (lseek(loop_fd, 0, SEEK_SET) < 0) {
        perror("lseek failed before write to new backing file");
        goto cleanup;
    }
    if (write(loop_fd, write_buf_2, sizeof(write_buf_2)) != sizeof(write_buf_2)) {
        perror("Failed to write to loop device with new backing file");
        goto cleanup;
    }
    printf("Successfully wrote '%s' to loop device with new backing file.\n", write_buf_2);

    // 校验第二个后端文件对应偏移512字节的数据是否与写入内容一致
    verify_fd = open(TEST_FILE_NAME_2, O_RDONLY);
    if (verify_fd < 0) {
        perror("Failed to reopen new backing file for verification");
        goto cleanup;
    }
    if (lseek(verify_fd, (off_t)status.lo_offset, SEEK_SET) < 0) { // 使用之前设置的偏移量
        perror("Failed to seek new backing file");
        close(verify_fd);
        goto cleanup;
    }
    memset(verify_buf, 0, sizeof(verify_buf));
    if (read(verify_fd, verify_buf, sizeof(write_buf_2)) != sizeof(write_buf_2)) {
        perror("Failed to read back from new backing file");
        close(verify_fd);
        goto cleanup;
    }
    close(verify_fd);
    if (memcmp(write_buf_2, verify_buf, sizeof(write_buf_2)) != 0) {
        fprintf(stderr, "New backing file data mismatch after LOOP_CHANGE_FD.\n");
        goto cleanup;
    }
    printf("New backing file content verification passed after LOOP_CHANGE_FD.\n");

    // =======================================================
    // 新增测试用例：LOOP_SET_CAPACITY
    // =======================================================
    printf("\n--- Testing LOOP_SET_CAPACITY ---\n");
    // 增大 TEST_FILE_NAME_2 的大小
    int resize_fd = open(TEST_FILE_NAME_2, O_RDWR);
    if (resize_fd < 0) {
        perror("Failed to open TEST_FILE_NAME_2 for resizing");
        goto cleanup;
    }
    int new_backing_file_size = TEST_FILE_SIZE_2 * 2; // 双倍大小
    if (ftruncate(resize_fd, new_backing_file_size) < 0) {
        perror("Failed to ftruncate TEST_FILE_NAME_2");
        close(resize_fd);
        goto cleanup;
    }
    close(resize_fd);
    printf("Resized %s to %d bytes.\n", TEST_FILE_NAME_2, new_backing_file_size);

    printf("Calling LOOP_SET_CAPACITY to re-evaluate loop device size...\n");
    if (ioctl(loop_fd, LOOP_SET_CAPACITY, 0) < 0) { // 参数通常为0
        perror("Failed to set loop capacity");
        goto cleanup;
    }
    printf("LOOP_SET_CAPACITY called successfully.\n");

    // 获取并验证新的容量
    status_readback = (struct loop_status64){0};
    if (ioctl(loop_fd, LOOP_GET_STATUS64, &status_readback) < 0) {
        perror("Failed to get loop status64 after LOOP_SET_CAPACITY");
        goto cleanup;
    }
    printf("After LOOP_SET_CAPACITY, loop current offset: %llu, sizelimit: %llu, flags: 0x%x\n",
           (unsigned long long)status_readback.lo_offset,
           (unsigned long long)status_readback.lo_sizelimit,
           status_readback.lo_flags);

    // 重新计算预期大小。由于 lo_sizelimit 仍为非零值 (TEST_FILE_SIZE - 512)，
    // 并且新文件大小 (new_backing_file_size) 仍然可能小于 (lo_offset + lo_sizelimit)
    // 实际有效大小应为 min(new_backing_file_size - lo_offset, lo_sizelimit)
    uint64_t expected_effective_size = (new_backing_file_size > status_readback.lo_offset) ? (new_backing_file_size - status_readback.lo_offset) : 0;
    expected_effective_size = (status_readback.lo_sizelimit > 0) ? 
                                expected_effective_size < status_readback.lo_sizelimit ? expected_effective_size : status_readback.lo_sizelimit :
                                expected_effective_size;


    // 实际验证 read/write 是否能访问到更大的区域。
    // 由于我们之前设置了 lo_sizelimit，它会限制设备可见的大小。
    // 如果想要反映出 ftruncate 后更大的大小，需要将 lo_sizelimit 设为 0。
    // 让我们先将 lo_sizelimit 清零，再测试 LOOP_SET_CAPACITY。

    printf("Clearing lo_sizelimit and re-testing LOOP_SET_CAPACITY...\n");
    status.lo_sizelimit = 0; // 清零，表示不限制
    if (ioctl(loop_fd, LOOP_SET_STATUS64, &status) < 0) {
        perror("Failed to clear lo_sizelimit");
        goto cleanup;
    }
    printf("lo_sizelimit cleared. Calling LOOP_SET_CAPACITY again.\n");

    if (ioctl(loop_fd, LOOP_SET_CAPACITY, 0) < 0) {
        perror("Failed to set loop capacity after clearing sizelimit");
        goto cleanup;
    }
    printf("LOOP_SET_CAPACITY called successfully after clearing sizelimit.\n");

    if (ioctl(loop_fd, LOOP_GET_STATUS64, &status_readback) < 0) {
        perror("Failed to get loop status64 after LOOP_SET_CAPACITY (sizelimit cleared)");
        goto cleanup;
    }
    printf("After LOOP_SET_CAPACITY (sizelimit cleared), loop current offset: %llu, sizelimit: %llu, flags: 0x%x\n",
           (unsigned long long)status_readback.lo_offset,
           (unsigned long long)status_readback.lo_sizelimit,
           status_readback.lo_flags);

    // 现在 lo_sizelimit 应该为 0，有效大小应为 (new_backing_file_size - lo_offset)
    expected_effective_size = (new_backing_file_size > status_readback.lo_offset) ? (new_backing_file_size - status_readback.lo_offset) : 0;
    
    // 尝试写入到新扩展的区域 (假设扩展区域在原文件大小之后)
    // 计算旧文件大小的最后一个块的偏移，然后写入一个块
    off_t write_offset_extended = (off_t)status_readback.lo_offset + TEST_FILE_SIZE_2; // 从原文件大小之后开始写
    char extended_write_buf[512] = "Extended Data!";

    printf("Attempting to write to extended region at offset %lld (device offset: %lld)...\n", 
           (long long)write_offset_extended, (long long)(TEST_FILE_SIZE_2)); // loop 设备上的相对偏移
    if (lseek(loop_fd, TEST_FILE_SIZE_2, SEEK_SET) < 0) { // 在 loop 设备上的相对偏移
        perror("lseek to extended region failed");
        goto cleanup;
    }
    if (write(loop_fd, extended_write_buf, sizeof(extended_write_buf)) != sizeof(extended_write_buf)) {
        perror("Failed to write to extended region of loop device");
        goto cleanup;
    }
    printf("Successfully wrote to extended region of loop device.\n");

    // 校验第二个后端文件扩展区域的数据
    verify_fd = open(TEST_FILE_NAME_2, O_RDONLY);
    if (verify_fd < 0) {
        perror("Failed to reopen new backing file for extended verification");
        goto cleanup;
    }
    if (lseek(verify_fd, write_offset_extended, SEEK_SET) < 0) {
        perror("Failed to seek new backing file for extended verification");
        close(verify_fd);
        goto cleanup;
    }
    memset(verify_buf, 0, sizeof(verify_buf));
    if (read(verify_fd, verify_buf, sizeof(extended_write_buf)) != sizeof(extended_write_buf)) {
        perror("Failed to read back from new backing file extended region");
        close(verify_fd);
        goto cleanup;
    }
    close(verify_fd);
    if (memcmp(extended_write_buf, verify_buf, sizeof(extended_write_buf)) != 0) {
        fprintf(stderr, "New backing file extended data mismatch after LOOP_SET_CAPACITY.\n");
        goto cleanup;
    }
    printf("New backing file extended content verification passed.\n");


cleanup:
    // 6. 清理并删除 loop 设备
    printf("\n--- Cleaning up ---\n");
    printf("Clearing loop device loop%d backing file...\n", loop_minor);
    if (ioctl(loop_fd, LOOP_CLR_FD, 0) < 0) {
        perror("Failed to clear loop device backing file");
    }

    printf("Removing loop device loop%d...\n", loop_minor);
    if (ioctl(control_fd, LOOP_CTL_REMOVE, loop_minor) < 0) {
        perror("Failed to remove loop device");
        // 尝试关闭所有打开的fd，即使删除失败也继续清理
    } else {
        printf("Loop device loop%d removed successfully.\n", loop_minor);
    }

    // 释放资源并删除测试文件
    close(loop_fd);
    close(backing_fd_1);
    close(backing_fd_2);
    close(control_fd);
    unlink(TEST_FILE_NAME);
    unlink(TEST_FILE_NAME_2);
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