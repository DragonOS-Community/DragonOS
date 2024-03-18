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
    // now has directory /ram/dir/dir/dir/dir
    char* file_path = malloc((3 + 4 * 4 + 5) * sizeof(char));
    strcpy(file_path, "ram");
    for(int j=0;j<4;j++) {
        strcpy(file_path + (4*j + 3) * sizeof(char), "/dir");
    }
    strcpy(file_path + (4*4 + 3) * sizeof(char), "/test");

    int fd = open(file_path, O_CREAT | O_WRONLY, S_IRUSR | S_IWUSR);
    if (fd == -1) {
        printf("Failed to open file for writing! Error: %s\n", strerror(errno));
        free(file_path);
        return 1;
    }

    const char* content = "Hello, World!";
    ssize_t bytes_written = write(fd, content, strlen(content));
    if (bytes_written == -1) {
        printf("Failed to write to file! Error: %s\n", strerror(errno));
        close(fd);
        free(file_path);
        return 1;
    }

    close(fd);

    fd = open(file_path, O_RDONLY);
    if (fd == -1) {
        printf("Failed to open file for reading! Error: %s\n", strerror(errno));
        free(file_path);
        return 1;
    }

    char buffer[100];
    ssize_t bytes_read = read(fd, buffer, sizeof(buffer) - 1);
    if (bytes_read == -1) {
        printf("Failed to read from file! Error: %s\n", strerror(errno));
        close(fd);
        free(file_path);
        return 1;
    }

    buffer[bytes_read] = '\0';
    printf("Read from file: %s\n", buffer);

    close(fd);

    if (remove(file_path) == -1) {
        printf("Failed to delete file! Error: %s\n", strerror(errno));
        free(file_path);
        return 1;
    }

    free(file_path);
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