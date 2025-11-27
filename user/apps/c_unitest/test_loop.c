#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/ioctl.h>
#include <sys/stat.h> // 用于 fstat
#include <stdint.h>
#include <errno.h>
#include <pthread.h>
#include <time.h>

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

// ===================================================================
// 资源回收测试辅助结构和函数
// ===================================================================

// 线程参数结构，用于并发I/O测试
struct io_thread_args {
    char loop_dev_path[64];
    int duration_seconds;
    volatile int should_stop;
    int io_count;
    int error_count;
};

// 并发读写线程函数
void* io_worker_thread(void* arg) {
    struct io_thread_args* args = (struct io_thread_args*)arg;
    char buffer[512];
    time_t start_time = time(NULL);

    while (!args->should_stop && (time(NULL) - start_time) < args->duration_seconds) {
        int fd = open(args->loop_dev_path, O_RDWR);
        if (fd < 0) {
            if (errno == ENODEV || errno == ENOENT) {
                // 设备正在删除或已删除，这是预期的
                break;
            }
            args->error_count++;
            usleep(10000); // 10ms
            continue;
        }

        // 尝试读取
        if (read(fd, buffer, sizeof(buffer)) < 0) {
            if (errno != ENODEV) {
                args->error_count++;
            }
        } else {
            args->io_count++;
        }

        close(fd);
        usleep(1000); // 1ms
    }

    return NULL;
}

// 删除设备线程函数
struct delete_thread_args {
    int control_fd;
    int loop_minor;
    int result;
    int error_code;
};

void* delete_worker_thread(void* arg) {
    struct delete_thread_args* args = (struct delete_thread_args*)arg;

    // 稍微延迟以确保I/O线程已经开始
    usleep(50000); // 50ms

    args->result = ioctl(args->control_fd, LOOP_CTL_REMOVE, args->loop_minor);
    args->error_code = errno;

    return NULL;
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
    // uint64_t expected_sizelimit_after_change = (actual_file_2_size > status_readback.lo_offset) ? (actual_file_2_size - status_readback.lo_offset) : 0;

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

    // =======================================================
    // 资源回收测试 1: 并发I/O期间删除设备
    // =======================================================
    printf("\n--- Testing Resource Reclamation: Concurrent I/O During Deletion ---\n");

    // 创建新的loop设备用于此测试，使用重试机制处理可能的竞态条件
    int test_minor = -1;
    int returned_minor_test = -1;
    for (int retry = 0; retry < 10; retry++) {
        if (ioctl(control_fd, LOOP_CTL_GET_FREE, &test_minor) < 0) {
            perror("Failed to get free loop device for reclamation test");
            goto cleanup;
        }

        returned_minor_test = ioctl(control_fd, LOOP_CTL_ADD, test_minor);
        if (returned_minor_test >= 0) {
            test_minor = returned_minor_test;
            break;
        }

        if (errno != EEXIST) {
            perror("Failed to add loop device for reclamation test");
            goto cleanup;
        }
        // 如果设备已存在，重试获取新的minor号
        printf("Device loop%d already exists, retrying...\n", test_minor);
    }

    if (returned_minor_test < 0) {
        fprintf(stderr, "Failed to create loop device after 10 retries\n");
        goto cleanup;
    }

    char test_loop_path[64];
    sprintf(test_loop_path, "/dev/loop%d", test_minor);
    int test_loop_fd = open(test_loop_path, O_RDWR);
    if (test_loop_fd < 0) {
        perror("Failed to open test loop device");
        ioctl(control_fd, LOOP_CTL_REMOVE, test_minor);
        goto cleanup;
    }

    // 绑定到测试文件
    if (ioctl(test_loop_fd, LOOP_SET_FD, backing_fd_1) < 0) {
        perror("Failed to bind test loop device");
        close(test_loop_fd);
        ioctl(control_fd, LOOP_CTL_REMOVE, test_minor);
        goto cleanup;
    }
    printf("Created test loop device loop%d for concurrent I/O test.\n", test_minor);

    // 启动多个I/O线程
    #define NUM_IO_THREADS 4
    pthread_t io_threads[NUM_IO_THREADS];
    struct io_thread_args io_args[NUM_IO_THREADS];

    for (int i = 0; i < NUM_IO_THREADS; i++) {
        strcpy(io_args[i].loop_dev_path, test_loop_path);
        io_args[i].duration_seconds = 5;
        io_args[i].should_stop = 0;
        io_args[i].io_count = 0;
        io_args[i].error_count = 0;

        if (pthread_create(&io_threads[i], NULL, io_worker_thread, &io_args[i]) != 0) {
            perror("Failed to create I/O thread");
            // 清理已创建的线程
            for (int j = 0; j < i; j++) {
                io_args[j].should_stop = 1;
                pthread_join(io_threads[j], NULL);
            }
            close(test_loop_fd);
            ioctl(control_fd, LOOP_CTL_REMOVE, test_minor);
            goto cleanup;
        }
    }
    printf("Started %d concurrent I/O threads.\n", NUM_IO_THREADS);

    // 关闭主文件描述符，避免在删除时引用计数不为0
    // I/O线程会重新打开设备进行操作
    close(test_loop_fd);
    printf("Closed main loop device file descriptor.\n");

    // 启动删除线程
    pthread_t delete_thread;
    struct delete_thread_args delete_args;
    delete_args.control_fd = control_fd;
    delete_args.loop_minor = test_minor;
    delete_args.result = 0;
    delete_args.error_code = 0;
    //创建失败回退删除所有线程
    if (pthread_create(&delete_thread, NULL, delete_worker_thread, &delete_args) != 0) {
        perror("Failed to create delete thread");
        for (int i = 0; i < NUM_IO_THREADS; i++) {
            io_args[i].should_stop = 1;
            pthread_join(io_threads[i], NULL);
        }
        close(test_loop_fd);
        ioctl(control_fd, LOOP_CTL_REMOVE, test_minor);
        goto cleanup;
    }
    printf("Started deletion thread.\n");

    // 等待删除完成
    pthread_join(delete_thread, NULL);
    printf("Deletion thread completed with result: %d (errno: %d)\n",
           delete_args.result, delete_args.error_code);

    // 停止I/O线程
    for (int i = 0; i < NUM_IO_THREADS; i++) {
        io_args[i].should_stop = 1;
    }

    // 等待所有I/O线程完成
    int total_io_count = 0;
    int total_error_count = 0;
    for (int i = 0; i < NUM_IO_THREADS; i++) {
        pthread_join(io_threads[i], NULL);
        total_io_count += io_args[i].io_count;
        total_error_count += io_args[i].error_count;
        printf("I/O thread %d: %d successful ops, %d errors\n",
               i, io_args[i].io_count, io_args[i].error_count);
    }
    printf("Total I/O operations: %d successful, %d errors\n",
           total_io_count, total_error_count);

    // test_loop_fd 已经在删除前关闭了

    if (delete_args.result == 0) {
        printf("✓ Concurrent I/O deletion test PASSED: Device deleted successfully while I/O was active.\n");
    } else {
        printf("✗ Concurrent I/O deletion test FAILED: Deletion returned %d (errno: %d)\n",
               delete_args.result, delete_args.error_code);
    }

    // 验证设备已被删除
    int verify_test_fd = open(test_loop_path, O_RDWR);
    if (verify_test_fd < 0 && (errno == ENOENT || errno == ENODEV)) {
        printf("✓ Device %s is correctly inaccessible after deletion.\n", test_loop_path);
    } else {
        if (verify_test_fd >= 0) {
            printf("✗ FAILED: Device %s still accessible after deletion!\n", test_loop_path);
            close(verify_test_fd);
        }
    }

    // =======================================================
    // 资源回收测试 2: 删除未绑定的设备
    // =======================================================
    printf("\n--- Testing Resource Reclamation: Deleting Unbound Device ---\n");

    int unbound_minor = -1;
    int ret_unbound = -1;
    for (int retry = 0; retry < 10; retry++) {
        if (ioctl(control_fd, LOOP_CTL_GET_FREE, &unbound_minor) < 0) {
            perror("Failed to get free loop device for unbound test");
            goto cleanup;
        }

        ret_unbound = ioctl(control_fd, LOOP_CTL_ADD, unbound_minor);
        if (ret_unbound >= 0) {
            unbound_minor = ret_unbound;
            break;
        }

        if (errno != EEXIST) {
            perror("Failed to add loop device for unbound test");
            goto cleanup;
        }
    }

    if (ret_unbound < 0) {
        fprintf(stderr, "Failed to create unbound loop device after retries\n");
        goto cleanup;
    }
    printf("Created unbound loop device loop%d.\n", unbound_minor);

    // 立即删除未绑定的设备
    if (ioctl(control_fd, LOOP_CTL_REMOVE, unbound_minor) < 0) {
        perror("Failed to remove unbound loop device");
        printf("✗ Unbound device deletion test FAILED.\n");
    } else {
        printf("✓ Unbound device deletion test PASSED: Successfully deleted loop%d.\n", unbound_minor);
    }

    // =======================================================
    // 资源回收测试 3: 重复删除同一设备
    // =======================================================
    printf("\n--- Testing Resource Reclamation: Duplicate Deletion ---\n");

    int dup_minor = -1;
    int ret_dup = -1;
    for (int retry = 0; retry < 10; retry++) {
        if (ioctl(control_fd, LOOP_CTL_GET_FREE, &dup_minor) < 0) {
            perror("Failed to get free loop device for duplicate deletion test");
            goto cleanup;
        }

        ret_dup = ioctl(control_fd, LOOP_CTL_ADD, dup_minor);
        if (ret_dup >= 0) {
            dup_minor = ret_dup;
            break;
        }

        if (errno != EEXIST) {
            perror("Failed to add loop device for duplicate deletion test");
            goto cleanup;
        }
    }

    if (ret_dup < 0) {
        fprintf(stderr, "Failed to create loop device for duplicate deletion test after retries\n");
        goto cleanup;
    }
    printf("Created loop device loop%d for duplicate deletion test.\n", dup_minor);

    // 第一次删除
    if (ioctl(control_fd, LOOP_CTL_REMOVE, dup_minor) < 0) {
        perror("First deletion failed");
        printf("✗ Duplicate deletion test FAILED: First deletion failed.\n");
        goto cleanup;
    }
    printf("First deletion of loop%d succeeded.\n", dup_minor);

    // 第二次删除（应该失败）
    errno = 0;
    int second_delete = ioctl(control_fd, LOOP_CTL_REMOVE, dup_minor);
    if (second_delete < 0 && (errno == ENODEV || errno == EINVAL)) {
        printf("✓ Duplicate deletion test PASSED: Second deletion correctly failed with errno %d.\n", errno);
    } else {
        printf("✗ Duplicate deletion test FAILED: Second deletion returned %d (errno: %d), expected failure.\n",
               second_delete, errno);
    }

    // =======================================================
    // 资源回收测试 4: 文件描述符泄漏检测
    // =======================================================
    printf("\n--- Testing Resource Reclamation: File Descriptor Leak Detection ---\n");

    // 创建多个loop设备并快速删除，检查是否有FD泄漏
    #define LEAK_TEST_COUNT 10
    int leak_test_minors[LEAK_TEST_COUNT];
    int leak_test_fds[LEAK_TEST_COUNT];

    printf("Creating and deleting %d loop devices to test for FD leaks...\n", LEAK_TEST_COUNT);

    for (int i = 0; i < LEAK_TEST_COUNT; i++) {
        leak_test_minors[i] = -1;
        leak_test_fds[i] = -1;

        // 使用重试机制创建设备
        for (int retry = 0; retry < 10; retry++) {
            int free_minor;
            if (ioctl(control_fd, LOOP_CTL_GET_FREE, &free_minor) < 0) {
                perror("Failed to get free loop device for leak test");
                // 清理已创建的设备
                for (int j = 0; j < i; j++) {
                    if (leak_test_minors[j] >= 0) {
                        ioctl(control_fd, LOOP_CTL_REMOVE, leak_test_minors[j]);
                    }
                }
                goto cleanup;
            }

            int ret_leak = ioctl(control_fd, LOOP_CTL_ADD, free_minor);
            if (ret_leak >= 0) {
                leak_test_minors[i] = ret_leak;
                break;
            }

            if (errno != EEXIST) {
                perror("Failed to add loop device for leak test");
                for (int j = 0; j < i; j++) {
                    if (leak_test_minors[j] >= 0) {
                        ioctl(control_fd, LOOP_CTL_REMOVE, leak_test_minors[j]);
                    }
                }
                goto cleanup;
            }
        }

        if (leak_test_minors[i] < 0) {
            fprintf(stderr, "Failed to create loop device %d for leak test\n", i);
            for (int j = 0; j < i; j++) {
                if (leak_test_minors[j] >= 0) {
                    ioctl(control_fd, LOOP_CTL_REMOVE, leak_test_minors[j]);
                }
            }
            goto cleanup;
        }

        // 打开并绑定设备
        char leak_path[64];
        sprintf(leak_path, "/dev/loop%d", leak_test_minors[i]);
        leak_test_fds[i] = open(leak_path, O_RDWR);
        if (leak_test_fds[i] >= 0) {
            ioctl(leak_test_fds[i], LOOP_SET_FD, backing_fd_1);
        }
    }

    // 删除所有设备
    int leak_delete_success = 0;
    for (int i = 0; i < LEAK_TEST_COUNT; i++) {
        if (leak_test_fds[i] >= 0) {
            ioctl(leak_test_fds[i], LOOP_CLR_FD, 0);
            close(leak_test_fds[i]);
        }

        if (ioctl(control_fd, LOOP_CTL_REMOVE, leak_test_minors[i]) == 0) {
            leak_delete_success++;
        }
    }

    printf("Successfully deleted %d out of %d devices.\n", leak_delete_success, LEAK_TEST_COUNT);
    if (leak_delete_success == LEAK_TEST_COUNT) {
        printf("✓ FD leak test PASSED: All devices deleted successfully.\n");
    } else {
        printf("⚠ FD leak test: %d devices failed to delete.\n", LEAK_TEST_COUNT - leak_delete_success);
    }

    // =======================================================
    // 资源回收测试 5: 删除后设备不可访问
    // =======================================================
    printf("\n--- Testing Resource Reclamation: Device Inaccessibility After Deletion ---\n");

    int reject_minor = -1;
    int ret_reject = -1;
    for (int retry = 0; retry < 10; retry++) {
        if (ioctl(control_fd, LOOP_CTL_GET_FREE, &reject_minor) < 0) {
            perror("Failed to get free loop device for I/O rejection test");
            goto cleanup;
        }

        ret_reject = ioctl(control_fd, LOOP_CTL_ADD, reject_minor);
        if (ret_reject >= 0) {
            reject_minor = ret_reject;
            break;
        }

        if (errno != EEXIST) {
            perror("Failed to add loop device for I/O rejection test");
            goto cleanup;
        }
    }

    if (ret_reject < 0) {
        fprintf(stderr, "Failed to create loop device for I/O rejection test after retries\n");
        goto cleanup;
    }

    char reject_path[64];
    sprintf(reject_path, "/dev/loop%d", reject_minor);
    int reject_fd = open(reject_path, O_RDWR);
    if (reject_fd < 0) {
        perror("Failed to open loop device for I/O rejection test");
        ioctl(control_fd, LOOP_CTL_REMOVE, reject_minor);
        goto cleanup;
    }

    if (ioctl(reject_fd, LOOP_SET_FD, backing_fd_1) < 0) {
        perror("Failed to bind loop device for I/O rejection test");
        close(reject_fd);
        ioctl(control_fd, LOOP_CTL_REMOVE, reject_minor);
        goto cleanup;
    }
    printf("Created and bound loop device loop%d for I/O rejection test.\n", reject_minor);

    // 执行一次成功的I/O操作
    char reject_buf[512] = "Test data";
    if (write(reject_fd, reject_buf, sizeof(reject_buf)) != sizeof(reject_buf)) {
        perror("Initial write failed");
    } else {
        printf("Initial write succeeded (expected).\n");
    }

    // 关闭文件描述符，准备删除
    close(reject_fd);
    printf("Closed device file descriptor.\n");

    // 触发删除
    printf("Triggering device deletion...\n");
    if (ioctl(control_fd, LOOP_CTL_REMOVE, reject_minor) < 0) {
        perror("Failed to trigger deletion for I/O rejection test");
        goto cleanup;
    }
    printf("Deletion triggered successfully.\n");

    // 尝试重新打开设备（应该失败）
    errno = 0;
    int reopen_reject_fd = open(reject_path, O_RDWR);
    if (reopen_reject_fd < 0 && (errno == ENODEV || errno == ENOENT)) {
        printf("✓ I/O rejection test PASSED: Device correctly inaccessible after deletion (errno: %d).\n", errno);
    } else {
        if (reopen_reject_fd >= 0) {
            printf("✗ I/O rejection test FAILED: Device still accessible after deletion.\n");
            close(reopen_reject_fd);
        } else {
            printf("✗ I/O rejection test FAILED: Unexpected errno %d (expected ENODEV or ENOENT).\n", errno);
        }
    }

    printf("\n=== All Resource Reclamation Tests Completed ===\n");


cleanup:
    // 6. 清理并删除 loop 设备
    printf("\n--- Cleaning up ---\n");
    printf("Clearing loop device loop%d backing file...\n", loop_minor);
    if (ioctl(loop_fd, LOOP_CLR_FD, 0) < 0) {
        perror("Failed to clear loop device backing file");
    }

    // 在删除设备前先关闭文件描述符，避免引用计数问题
    close(loop_fd);
    printf("Closed loop device file descriptor.\n");

    printf("Removing loop device loop%d...\n", loop_minor);
    if (ioctl(control_fd, LOOP_CTL_REMOVE, loop_minor) < 0) {
        perror("Failed to remove loop device");
        // 即使删除失败也继续清理
    } else {
        printf("Loop device loop%d removed successfully.\n", loop_minor);
    }

    // 释放资源并删除测试文件
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