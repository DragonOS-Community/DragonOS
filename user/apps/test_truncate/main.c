#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <sys/types.h>
#include <sys/stat.h>
#include <fcntl.h>
#include <string.h>

int main(int argc, char *argv[]) {
    if (argc != 3) {
        fprintf(stderr, "用法: %s <文件名> <新大小(字节)>\n", argv[0]);
        exit(EXIT_FAILURE);
    }

    const char *filename = argv[1];
    off_t new_size = atoi(argv[2]);
    struct stat file_stat;
    
    // 首先获取原始文件大小
    if (stat(filename, &file_stat) == -1) {
        perror("获取原始文件大小失败");
        exit(EXIT_FAILURE);
    }
    printf("原始文件大小: %ld字节\n", (long)file_stat.st_size);
    
    // 使用truncate()函数 - 通过文件名操作
    if (truncate(filename, new_size) == -1) {
        perror("truncate()失败");
        exit(EXIT_FAILURE);
    }
    printf("使用truncate()成功将文件'%s'大小设置为%ld字节\n", filename, (long)new_size);
    
    // 再次获取文件大小验证
    if (stat(filename, &file_stat) == -1) {
        perror("验证文件大小失败");
        exit(EXIT_FAILURE);
    }
    
    if (file_stat.st_size == new_size) {
        printf("验证成功: 文件大小确实已更改为%ld字节\n", (long)new_size);
    } else {
        printf("验证失败: 当前文件大小为%ld字节，与目标大小%ld字节不符\n", 
               (long)file_stat.st_size, (long)new_size);
        exit(EXIT_FAILURE);
    }
    
    return EXIT_SUCCESS;
}
