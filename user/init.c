#include <libc/unistd.h>
#include <libc/stdio.h>
#include <libc/fcntl.h>
#include <libc/stdlib.h>

int main()
{

    char string[] = "333.txt";
    uint8_t buf[128] = {0};
    char tips_str[] = "The first application 'init.bin' started successfully!\n";
    put_string(tips_str, COLOR_GREEN, COLOR_BLACK);

    printf("test printf: %s size: %d\n", string, sizeof(string));
    /*
    int fd = open(string, 0);
    printf("fd=%d\n", fd);

    read(fd, buf, 128);

    put_string(buf, COLOR_ORANGE, COLOR_BLACK);

    lseek(fd, 0, SEEK_SET);
    write(fd, tips_str, sizeof(tips_str)-1);
    lseek(fd, 0, SEEK_SET);

    // 由于暂时没有实现用户态的memset，因此先手动清零
    for(int i=0;i<128;++i)
        buf[i] = 0;

    read(fd, buf, 128);
    put_string(buf, COLOR_YELLOW, COLOR_BLACK);
    close(fd);
    */

    void *ptr[256] = {0};
    for (int k = 0; k < 2; ++k)
    {
        printf("try to malloc 256*16K=4MB\n");
        uint64_t js = 0;
        for (int i = 0; i < 256; ++i)
        {
            ptr[i] = malloc(4096 * 4);
            js += *(uint64_t *)((uint64_t)(ptr[i]) - sizeof(uint64_t));
            if (*(uint64_t *)((uint64_t)(ptr[i]) - sizeof(uint64_t)) > 0x4008)
                printf("[%d] start_addr = %#018lx, len = %#010lx\n", (uint64_t)(ptr[i]) - 8, *(uint64_t *)((uint64_t)(ptr[i]) - sizeof(uint64_t)));
        }
        printf("ptr[0]->len=%lld\n", *(uint64_t *)((uint64_t)ptr[0] - sizeof(uint64_t)));
        printf("ptr[1]->len=%lld\n", *(uint64_t *)((uint64_t)ptr[1] - sizeof(uint64_t)));
        // printf("ptr[24]->len=%lld\n", *(uint64_t*)((uint64_t)ptr[24] - sizeof(uint64_t)));
        printf("alloc done. total used: %lld bytes\n", js);
        printf("try to free...\n");
        for (int i = 0; i < 256; ++i)
        {
            free(ptr[i]);
        }
        printf("free done!\n");
    }

    // *p = 'a';
    /*
    pid_t p = fork();
    if(p == 0)
        put_string("subproc\n", COLOR_PURPLE, COLOR_BLACK);
    else put_string("parent proc\n", COLOR_ORANGE, COLOR_BLACK);
*/
    while (1)
        ;
}