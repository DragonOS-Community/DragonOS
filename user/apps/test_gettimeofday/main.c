#include <sys/time.h>
#include <stdio.h>
#include <stdlib.h>
// #include <sleep.h>
// #include <unistd.h>
void main()
{
    struct timeval *tv = malloc(sizeof(struct timeval));
    struct timezone *tz = malloc(sizeof(struct timezone));
    gettimeofday(tv, tz);
    // struct timespec tm;
    // tm.tv_sec = 3;
    // tm.tv_nsec = 0;
    // nanosleep(&tm, NULL);
    // gettimeofday(tv, tz);

    // printf("tv_sec:%ld\n", tv->tv_sec);
    // printf("tz_minuteswest = %d,tz_dsttime = %d", (*tz).tz_minuteswest, (*tz).tz_dsttime);
    return;
}