#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/ioctl.h>
#include <sys/stat.h> // For fstat
// magic
#define LOOP_CTL_ADD        0x4C80
#define LOOP_CTL_REMOVE     0x4C81
#define LOOP_CTL_GET_FREE   0x4C82
#define LOOP_SET_FD         0x4C00
#define LOOP_CLR_FD         0x4C01
//调用open和ioctl（）接口实现通信
#define LOOP_DEVICE_CONTROL "/dev/loop-control"
#define TEST_FILE_NAME "test_image.img"
#define TEST_FILE_SIZE (1024 * 1024) // 1MB for the test image
//创建测试文件
void create_test_file() {
    printf("Creating test file: %s with size %d bytes\n", TEST_FILE_NAME, TEST_FILE_SIZE);
    int fd = open(TEST_FILE_NAME, O_CREAT | O_TRUNC | O_RDWR, 0644);
    if (fd < 0) {
        perror("Failed to create test file");
        exit(EXIT_FAILURE);
    }
    // Write some data to make it a non-empty file
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
    char write_buf[512] = "Hello Loop Device!";
    char read_buf[512];

    create_test_file(); // Create a file to back a loop device

    backing_fd = open(TEST_FILE_NAME, O_RDWR);
    if (backing_fd < 0) {
        perror("Failed to open backing file");
        exit(EXIT_FAILURE);
    }

    // 1. Open the loop-control device
    printf("Opening loop control device: %s\n", LOOP_DEVICE_CONTROL);
    control_fd = open(LOOP_DEVICE_CONTROL, O_RDWR);
    if (control_fd < 0) {
        perror("Failed to open loop control device. Make sure your kernel module is loaded and /dev/loop-control exists.");
        close(backing_fd);
        exit(EXIT_FAILURE);
    }
    printf("Loop control device opened successfully (fd=%d).\n", control_fd);

    // 2. Get a free loop device minor number
    printf("Requesting a free loop device minor...\n");
    if (ioctl(control_fd, LOOP_CTL_GET_FREE, &loop_minor) < 0) {
        perror("Failed to get free loop device minor");
        close(backing_fd);
        close(control_fd);
        exit(EXIT_FAILURE);
    }
    printf("Got free loop minor: %d\n", loop_minor);

    // 3. Add a new loop device using the minor number
    // Note: The `LOOP_CTL_ADD` in your Rust code takes the minor as `data: usize`.
    // We'll pass `loop_minor` directly. Your Rust code then creates an empty device.
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

    // 4. Try to open the newly created loop block device
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

    // 5. Test read/write operations on the loop block device

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

    // 6. Remove the loop device
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

    // Clean up
    close(loop_fd);
    close(backing_fd);
    close(control_fd);
    unlink(TEST_FILE_NAME);
    printf("All tests completed. Cleaned up.\n");

    return 0;
}
