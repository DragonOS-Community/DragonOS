#include <sys/types.h>
#include <unistd.h>
#include <sys/stat.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
int main()
{

    int fd = open("/bin/about.elf", O_RDONLY);
    if (fd == -1)
        return 0;
    printf("fd = %d\n", fd);
    // struct stat *st = (struct stat *)malloc(sizeof(struct stat));
    struct stat *st = (struct stat *)malloc(256);
    fstat(fd, st);
    // FIXME 打印数据时内存出错
    // printf("====================\n");
    // printf("st address: %#018lx\n", st);
    // printf("stat:st_dev = %d\n st_ino = %d\n st_mode = %d\n st_nlink = %d\n st_uid = %d\n st_gid = %d\n st_rdev = %d\n st_size = %d\n st_blksize = %d\n st_blocks = %d\n ",
    //        (*st).st_dev, (*st).st_ino, (*st).st_mode, (*st).st_nlink, (*st).st_uid, (*st).st_gid, (*st).st_rdev, (*st).st_size, (*st).st_blksize, (*st).st_blocks);
    // printf("st_atim.sec= %d\tst_atim.nsec= %d\n", (*st).st_atim.tv_sec, (*st).st_atim.tv_nsec);
    // printf("st_mtim.sec= %d\tst_mtim.nsec= %d\n", (*st).st_mtim.tv_sec, (*st).st_mtim.tv_nsec);
    // printf("st_ctim.sec= %d\tst_ctim.nsec= %d\n", (*st).st_ctim.tv_sec, (*st).st_ctim.tv_nsec);

    return 0;
}