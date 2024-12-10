#include <sys/time.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <time.h>

int main()
{
    struct timeval *tv = malloc(sizeof(struct timeval));
    struct timezone *tz = malloc(sizeof(struct timezone));
    for (int i = 0; i < 15; i++)
    {
        gettimeofday(tv, NULL);
        printf("%ld.%06ld\n", tv->tv_sec, tv->tv_usec);
        for (int i = 0; i < 10; i++)
        {
            usleep(500000);
        }
    }
    gettimeofday(tv, NULL);
    printf("tv = %ld.%06ld\n", tv->tv_sec, tv->tv_usec);
    // printf("tz_minuteswest = %d,tz_dsttime = %d", (*tz).tz_minuteswest, (*tz).tz_dsttime);
    return 0;
}