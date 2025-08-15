#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <unistd.h>
#include <string.h>

int main() {
    size_t pagesize = sysconf(_SC_PAGESIZE);
    size_t npages = 10;
    size_t length = pagesize * npages;

    printf("11111111111111111");
    // 匿名映射一段内存
    char *addr = mmap(NULL, length, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr == MAP_FAILED) {
        perror("mmap");
        return 1;
    }
     printf("2222222222222222222");
    // 写入数据，确保物理页被分配
    memset(addr, 0xaa, length);

    printf("33333333333333333");
    // 分配mincore结果数组
    unsigned char *vec = malloc(npages);
    if (!vec) {
        perror("malloc");
        munmap(addr, length);
        return 1;
    }
     printf("4444444444444444444444");
    // 调用mincore
    if (mincore(addr, length, vec) == -1) {
        perror("mincore");
        free(vec);
        munmap(addr, length);
        return 1;
    }
    printf("55555555555555555555555");

    // 输出每一页的驻留情况
    for (size_t i = 0; i < npages; i++) {
        printf("Page %zu: %s\n", i, (vec[i] & 1) ? "In core" : "Not in core");
    }

    free(vec);
    munmap(addr, length);
    return 0;
}