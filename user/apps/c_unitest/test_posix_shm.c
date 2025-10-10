#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>

#define SHM_NAME "/test_posix_shm"
#define SHM_SIZE 4096

int main() {
    int fd;
    char *addr;
    const char *test_data = "Hello, POSIX Shared Memory!";
    char read_buffer[256];

    printf("=== POSIX 共享内存测试开始 ===\n");

    // 测试 shm_open - 创建共享内存对象
    printf("1. 测试 shm_open (创建)...\n");
    fd = shm_open(SHM_NAME, O_CREAT | O_RDWR, 0666);
    if (fd == -1) {
        perror("shm_open failed");
        exit(EXIT_FAILURE);
    }
    printf("   shm_open 成功，fd = %d\n", fd);

    // 设置共享内存大小
    printf("2. 设置共享内存大小...\n");
    if (ftruncate(fd, SHM_SIZE) == -1) {
        perror("ftruncate failed");
        close(fd);
        shm_unlink(SHM_NAME);
        exit(EXIT_FAILURE);
    }
    printf("   ftruncate 成功，大小 = %d 字节\n", SHM_SIZE);

    // 映射共享内存
    printf("3. 映射共享内存...\n");
    addr = mmap(NULL, SHM_SIZE, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (addr == MAP_FAILED) {
        perror("mmap failed");
        close(fd);
        shm_unlink(SHM_NAME);
        exit(EXIT_FAILURE);
    }
    printf("   mmap 成功，地址 = %p\n", addr);

    // 写入数据
    printf("4. 写入测试数据...\n");
    strcpy(addr, test_data);
    printf("   写入数据: \"%s\"\n", test_data);

    // 读取数据验证
    printf("5. 读取数据验证...\n");
    strcpy(read_buffer, addr);
    printf("   读取数据: \"%s\"\n", read_buffer);
    
    if (strcmp(test_data, read_buffer) == 0) {
        printf("   ✓ 数据验证成功！\n");
    } else {
        printf("   ✗ 数据验证失败！\n");
    }

    // 测试重新打开已存在的共享内存
    printf("6. 测试重新打开已存在的共享内存...\n");
    int fd2 = shm_open(SHM_NAME, O_RDWR, 0);
    if (fd2 == -1) {
        perror("shm_open (existing) failed");
    } else {
        printf("   重新打开成功，fd2 = %d\n", fd2);
        close(fd2);
    }

    // 清理资源
    printf("7. 清理资源...\n");
    
    // 取消映射
    if (munmap(addr, SHM_SIZE) == -1) {
        perror("munmap failed");
    } else {
        printf("   munmap 成功\n");
    }

    // 关闭文件描述符
    if (close(fd) == -1) {
        perror("close failed");
    } else {
        printf("   close 成功\n");
    }

    // 测试 shm_unlink - 删除共享内存对象
    printf("8. 测试 shm_unlink...\n");
    if (shm_unlink(SHM_NAME) == -1) {
        perror("shm_unlink failed");
        exit(EXIT_FAILURE);
    }
    printf("   shm_unlink 成功\n");

    // 验证删除后无法再次打开
    printf("9. 验证删除后无法再次打开...\n");
    int fd3 = shm_open(SHM_NAME, O_RDWR, 0);
    if (fd3 == -1) {
        printf("   ✓ 验证成功：删除后无法打开 (errno = %d)\n", errno);
    } else {
        printf("   ✗ 验证失败：删除后仍能打开\n");
        close(fd3);
    }

    printf("=== POSIX 共享内存测试完成 ===\n");
    return 0;
}