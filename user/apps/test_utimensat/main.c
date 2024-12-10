#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>


int main(){
    int res = utimensat(AT_FDCWD, "/bin/about.elf", NULL, 0);
    printf("utimensat res = %d\n", res);
}