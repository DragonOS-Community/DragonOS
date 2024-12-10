#include <sys/mman.h>
#include <sys/types.h>
#include <sys/stat.h>
#include <fcntl.h>
#include <unistd.h>
#include <stdio.h>
#include <stdlib.h>

int main()
{
    // 打开文件
    int fd = open("example.txt", O_RDWR | O_CREAT | O_TRUNC, 0777);

    if (fd == -1)
    {
        perror("open");
        exit(EXIT_FAILURE);
    }

    write(fd, "HelloWorld!", 11);
    char buf[12];
    buf[11] = '\0';
    close(fd);

    fd = open("example.txt", O_RDWR);
    read(fd, buf, 11);
    printf("File content: %s\n", buf);

    // 将文件映射到内存
    void *map = mmap(NULL, 11, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (map == MAP_FAILED)
    {
        perror("mmap");
        close(fd);
        exit(EXIT_FAILURE);
    }
    printf("mmap address: %p\n", map);

    // 关闭文件描述符
    // close(fd);

    // 访问和修改文件内容
    char *fileContent = (char *)map;
    printf("change 'H' to 'G'\n");
    fileContent[0] = 'G'; // 修改第一个字符为 'G'
    printf("mmap content: %s\n", fileContent);

    // 解除映射
    printf("unmap\n");
    if (munmap(map, 11) == -1)
    {
        perror("munmap");
        exit(EXIT_FAILURE);
    }

    fd = open("example.txt", O_RDWR);
    read(fd, buf, 11);
    printf("File content: %s\n", buf);

    return 0;
}
