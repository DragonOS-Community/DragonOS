#include <stdio.h>
#include <stdlib.h>
#include <fcntl.h>
#include <sys/stat.h>
#include <unistd.h>
#include <string.h>
#include <sys/syscall.h>



#define TEST_DIR "test_dir"
#define TEST_FILE "test_file"

void create_test_files() {
    mkdir(TEST_DIR, 0755);
    int fd = open(TEST_FILE, O_CREAT | O_RDWR, 0644);
    if (fd >= 0) close(fd);
}

void cleanup_test_files() {
    unlink(TEST_FILE);
    rmdir(TEST_DIR);
}

void run_test(const char *name, int (*test_func)(), int expected) {
    printf("Testing %s... ", name);
    int result = test_func();
    if (result == expected) {
        printf("[PASS]\n");
    } else {
        printf("[FAILED] (expected %d, got %d)\n", expected, result);
    }
}

int test_normal_file() {
    struct stat st;
    return syscall(__NR_newfstatat, AT_FDCWD, TEST_FILE, &st, 0);
}

int test_directory() {
    struct stat st;
    return syscall(__NR_newfstatat, AT_FDCWD, TEST_DIR, &st, 0);
}

int test_invalid_fd() {
    struct stat st;
    return syscall(__NR_newfstatat, -1, TEST_FILE, &st, 0);
}

int test_nonexistent_path() {
    struct stat st;
    return syscall(__NR_newfstatat, AT_FDCWD, "nonexistent_file", &st, 0);
}

int main() {
    create_test_files();

    run_test("normal file stat", test_normal_file, 0);
    run_test("directory stat", test_directory, 0);
    run_test("invalid file descriptor", test_invalid_fd, -1);
    run_test("nonexistent path", test_nonexistent_path, -1);

    cleanup_test_files();
    return 0;
}
