#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/types.h>
#include <sys/stat.h>
#include <fcntl.h>
#include <unistd.h>
#include <string.h>

int main(int argc, char const *argv[])
{
    // char* path = malloc(4);
    char* path;
    char* old;
    int ret = 0;
    for(int i=0; i<5 && ret == 0; i++) {
        path = malloc((3 + 4 * i) * sizeof(char));
        strcpy(path, "ram");
        for(int j=0;j<i;j++) {
            strcpy(path + (4*j + 3) * sizeof(char), "/dir");
        }
        printf("Making Dir with path: %s\n", path);
        if (i != 0) ret = mkdir(path, S_IRWXU);
        free(path);
        if (i==0) continue;
        if ( ret == 0 ) {
            puts("Making success!");
        } else {
            printf("Making Failed! Error: %s", strerror(errno));
            break;
        }
    }
    for(int i=4; i>0 && ret == 0; i--) {
        path = malloc((3 + 4 * i) * sizeof(char));
        strcpy(path, "ram");
        for(int j=0;j<i;j++) {
            strcpy(path + (4*j + 3) * sizeof(char), "/dir");
        }
        printf("Remove Dir with path: %s\n", path);
        ret = rmdir(path);
        free(path);
        if ( ret == 0 ) {
            puts("Remove success!");
        } else {
            printf("Remove Failed! Error: %s", strerror(errno));
            break;
        }
    }
    return 0;
}