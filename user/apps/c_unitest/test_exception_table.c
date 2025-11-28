#include <stdio.h>
#include <unistd.h>
#include <sys/mman.h>
#include <string.h>
#include <errno.h>
#include <stdlib.h>
#include <fcntl.h>
#include <sys/stat.h>

int main() {
    printf("=== DragonOS Exception Table Test ===\n\n");
    
    // 测试1: open() 使用未映射的路径指针
    printf("Test 1: open() with unmapped path pointer\n");
    char *bad_path = (char *)0x1000;  // 未映射地址
    
    int fd = open(bad_path, O_RDONLY);
    if (fd == -1 && errno == EFAULT) {
        printf("  ✓ PASS: open returned -1 with EFAULT\n");
    } else {
        printf("  ✗ FAIL: open returned %d, errno=%d\n", fd, errno);
        if (fd >= 0) close(fd);
    }
    
    // 测试2: open() 使用已释放的内存作为路径
    printf("\nTest 2: open() with freed memory path\n");
    void *mem = mmap(NULL, 4096, PROT_READ | PROT_WRITE,
                     MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (mem == MAP_FAILED) {
        printf("  ✗ FAIL: mmap failed\n");
        return 1;
    }
    strcpy(mem, "/tmp/test");
    munmap(mem, 4096);
    
    fd = open(mem, O_RDONLY);
    if (fd == -1 && errno == EFAULT) {
        printf("  ✓ PASS: open returned -1 with EFAULT after munmap\n");
    } else {
        printf("  ✗ FAIL: open returned %d, errno=%d\n", fd, errno);
        if (fd >= 0) close(fd);
    }
    
    // 测试3: stat() 使用无效路径指针
    printf("\nTest 3: stat() with invalid path pointer\n");
    struct stat st;
    int ret = stat(bad_path, &st);
    if (ret == -1 && errno == EFAULT) {
        printf("  ✓ PASS: stat returned -1 with EFAULT\n");
    } else {
        printf("  ✗ FAIL: stat returned %d, errno=%d\n", ret, errno);
    }
    
    // 测试4: access() 使用无效路径指针
    printf("\nTest 4: access() with invalid path pointer\n");
    ret = access(bad_path, F_OK);
    if (ret == -1 && errno == EFAULT) {
        printf("  ✓ PASS: access returned -1 with EFAULT\n");
    } else {
        printf("  ✗ FAIL: access returned %d, errno=%d\n", ret, errno);
    }
    
    // 测试5: 正常的 open 应该工作
    printf("\nTest 5: normal open should work\n");
    fd = open("/", O_RDONLY);
    if (fd >= 0) {
        printf("  ✓ PASS: normal open succeeded (fd=%d)\n", fd);
        close(fd);
    } else {
        printf("  ✗ FAIL: normal open failed\n");
    }
    
    // 测试6: execve 使用无效的程序路径
    printf("\nTest 6: execve() with invalid path pointer\n");
    char *argv[] = { NULL };
    char *envp[] = { NULL };
    ret = execve(bad_path, argv, envp);
    if (ret == -1 && errno == EFAULT) {
        printf("  ✓ PASS: execve returned -1 with EFAULT\n");
    } else {
        printf("  ✗ FAIL: execve returned %d, errno=%d (should not reach here)\n", ret, errno);
    }
    
    printf("\n=== All tests completed ===\n");
    return 0;
}
