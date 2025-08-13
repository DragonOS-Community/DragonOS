#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>
int main()
{
    int fd = open("/bin/about.elf", O_RDONLY);
    if (fd == -1)
        return 0;
    printf("fd = %d\n", fd);
    struct stat *st = (struct stat *)malloc(sizeof(struct stat));
    fstat(fd, st);
    printf("stat size = %lu\n", sizeof(struct stat));
    // FIXME 打印数据时内存出错
    printf("====================\n");
    printf("st address: %p\n", st);
    printf("st_dev = %lu\n", (*st).st_dev);
    printf("st_ino = %lu\n", (*st).st_ino);
    printf("st_mode = %d\n", (*st).st_mode);
    printf("st_nlink = %lu\n", (*st).st_nlink);
    printf("st_uid = %d\n", (*st).st_uid);
    printf("st_gid = %d\n", (*st).st_gid);
    printf("st_rdev = %lu\n", (*st).st_rdev);
    printf("st_size = %ld\n", (*st).st_size);
    printf("st_blksize = %ld\n", (*st).st_blksize);
    printf("st_blocks = %ld\n", (*st).st_blocks);
    printf("st_atim.sec= %ld\tst_atim.nsec= %ld\n", (*st).st_atim.tv_sec, (*st).st_atim.tv_nsec);
    printf("st_mtim.sec= %ld\tst_mtim.nsec= %ld\n", (*st).st_mtim.tv_sec, (*st).st_mtim.tv_nsec);
    printf("st_ctim.sec= %ld\tst_ctim.nsec= %ld\n", (*st).st_ctim.tv_sec, (*st).st_ctim.tv_nsec);

    return 0;
}