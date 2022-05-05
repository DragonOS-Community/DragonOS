#include <libc/unistd.h>
#include <libc/stdio.h>
#include <libc/fcntl.h>

int main()
{

    char string[] = "333.txt";
    uint8_t buf[128] = {0};
    char tips_str[] = "The first application 'init.bin' started successfully!\n";
    put_string(tips_str, COLOR_GREEN, COLOR_BLACK);
    int fd = open(string, 0);
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

    while (1)
        ;
}