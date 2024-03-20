#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/types.h>
#include <sys/stat.h>
#include <fcntl.h>
#include <unistd.h>
#include <string.h>

#define MAX_PATH_LENGTH 100
#define MAX_DIR_DEPTH 4

void createDirectories() {
    char* path;
    int ret = 0;

    for (int i = 0; i <= MAX_DIR_DEPTH && ret == 0; i++) {
        path = malloc((4 + 4 * i) * sizeof(char));
        strcpy(path, "/ram");

        for (int j = 0; j < i; j++) {
            strcpy(path + (4 * j + 4) * sizeof(char), "/dir");
        }

        printf("Making Dir with path: %s\n", path);

        if (i != 0) {
            ret = mkdir(path, S_IRWXU);
        }

        free(path);

        if (i == 0) {
            continue;
        }

        if (ret == 0) {
            puts("Making success!");
        } else {
            printf("Making Failed! Error: %s", strerror(errno));
            break;
        }
    }
}

void createAndWriteToFile() {
    char* file_path = malloc((4 + 4 * MAX_DIR_DEPTH + 5) * sizeof(char));
    strcpy(file_path, "/ram");

    for (int j = 0; j < MAX_DIR_DEPTH; j++) {
        strcpy(file_path + (4 * j + 4) * sizeof(char), "/dir");
    }

    strcpy(file_path + (4 * MAX_DIR_DEPTH + 4) * sizeof(char), "/test");

    int fd = open(file_path, O_CREAT | O_WRONLY, S_IRUSR | S_IWUSR);

    if (fd == -1) {
        printf("Failed to open file for writing! Error: %s\n", strerror(errno));
        free(file_path);
        return;
    }

    const char* content = "Hello, World!";
    ssize_t bytes_written = write(fd, content, strlen(content));

    if (bytes_written == -1) {
        printf("Failed to write to file! Error: %s\n", strerror(errno));
        close(fd);
        free(file_path);
        return;
    }

    close(fd);
    free(file_path);
}

void readAndDeleteFile() {
    char* file_path = malloc((4 + 4 * MAX_DIR_DEPTH + 5) * sizeof(char));
    strcpy(file_path, "/ram");

    for (int j = 0; j < MAX_DIR_DEPTH; j++) {
        strcpy(file_path + (4 * j + 4) * sizeof(char), "/dir");
    }

    strcpy(file_path + (4 * MAX_DIR_DEPTH + 4) * sizeof(char), "/test");

    int fd = open(file_path, O_RDONLY);

    if (fd == -1) {
        printf("Failed to open file for reading! Error: %s\n", strerror(errno));
        free(file_path);
        return;
    }

    char buffer[MAX_PATH_LENGTH];
    ssize_t bytes_read = read(fd, buffer, sizeof(buffer) - 1);

    if (bytes_read == -1) {
        printf("Failed to read from file! Error: %s\n", strerror(errno));
        close(fd);
        free(file_path);
        return;
    }

    buffer[bytes_read] = '\0';
    printf("Read from file: %s\n", buffer);

    close(fd);

    if (remove(file_path) == -1) {
        printf("Failed to delete file! Error: %s\n", strerror(errno));
        free(file_path);
        return;
    }

    free(file_path);
}

void removeDirectories() {
    char* path;
    int ret = 0;

    for (int i = MAX_DIR_DEPTH; i > 0 && ret == 0; i--) {
        path = malloc((4 + 4 * i) * sizeof(char));
        strcpy(path, "/ram");

        for (int j = 0; j < i; j++) {
            strcpy(path + (4 * j + 4) * sizeof(char), "/dir");
        }

        printf("Remove Dir with path: %s\n", path);

        ret = rmdir(path);
        free(path);

        if (ret == 0) {
            puts("Remove success!");
        } else {
            printf("Remove Failed! Error: %s", strerror(errno));
            break;
        }
    }
}

int main(int argc, char const* argv[]) {
    createDirectories();
    createAndWriteToFile();
    readAndDeleteFile();
    // removeDirectories();

    return 0;
}