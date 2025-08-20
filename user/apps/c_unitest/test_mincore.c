#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <unistd.h>
#include <string.h>
#include <errno.h>

int main() {
    size_t pagesize = sysconf(_SC_PAGESIZE);
    size_t npages = 4;
    size_t length = pagesize * npages;

    // 匿名映射一段内存
    char *addr = mmap(NULL, length, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr == MAP_FAILED) {
        perror("mmap");
        printf("TEST FAILED: mmap failed\n");
        return 1;
    }

    // 写入数据，确保物理页被分配
    memset(addr, 0xaa, length);

    // 分配mincore结果数组
    unsigned char *vec = malloc(npages);
    if (!vec) {
        perror("malloc");
        munmap(addr, length);
        printf("TEST FAILED: malloc failed\n");
        return 1;
    }

    // 调用mincore
    if (mincore(addr, length, vec) == -1) {
        perror("mincore");
        free(vec);
        munmap(addr, length);
        printf("TEST FAILED: mincore syscall failed\n");
        return 1;
    }

    // 检查每一页是否驻留内存
    int success = 1;
    for (size_t i = 0; i < npages; i++) {
        printf("Page %zu: %s\n", i, (vec[i] & 1) ? "In core" : "Not in core");
        if (!(vec[i] & 1)) {
            success = 0;
        }
    }

    free(vec);
    munmap(addr, length);

    if (success) {
        printf("TEST PASSED: All pages are in core.\n");
        return 0;
    } else {
        printf("TEST FAILED: Some pages are not in core.\n");
        return 1;
    }
}