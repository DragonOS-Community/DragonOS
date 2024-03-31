#include <sys/statfs.h>
#include <stdio.h>

int main(int argc,char **argv)
{
    struct statfs diskInfo;

    
    statfs("/bin/about.elf", &diskInfo);
    unsigned long long blocksize1 = diskInfo.f_bsize;    //每个block里包含的字节数
    unsigned long long totalsize = blocksize1 * diskInfo.f_blocks;//总的字节数，f_blocks为block的数目
    printf("Total_size=%llu B =%llu KB =%llu MB = %llu GB\n",
           totalsize,totalsize>>10,totalsize>>20, totalsize>>30);

    /* 2.获取一下剩余空间和可用空间的大小 */
    unsigned long long freeDisk = diskInfo.f_bfree * blocksize1;  //剩余空间的大小 
    unsigned long long availableDisk = diskInfo.f_bavail * blocksize1; //可用空间大小
    printf("Disk_free=%llu MB =%llu GB Disk_available=%llu MB = %llu GB\n",
           freeDisk>>20,freeDisk>>30,availableDisk>>20, availableDisk>>30);


    printf("====================\n");
    printf("diskInfo address: %p\n", diskInfo);
    printf("f_type= %lu\n", diskInfo.f_type);
    printf("f_bsize = %lu\n", diskInfo.f_bsize);
    printf("f_blocks = %d\n", diskInfo.f_blocks);
    printf("f_bfree = %lu\n", diskInfo.f_bfree);
    printf("b_avail = %d\n", diskInfo.f_bavail);
    printf("f_files = %d\n", diskInfo.f_files);
    printf("f_ffree = %lu\n", diskInfo.f_ffree);
    printf("f_fsid = %ld\n", diskInfo.f_fsid);
    printf("f_namelen = %ld\n", diskInfo.f_namelen);
    printf("f_frsize = %ld\n", diskInfo.f_frsize);
    printf("f_flags = %ld\n", diskInfo.f_flags);
    return 0;
}