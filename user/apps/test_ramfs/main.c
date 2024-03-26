// #include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/types.h>
#include <sys/stat.h>
#include <fcntl.h>
#include <unistd.h>
#include <string.h>
#include <sys/mount.h>

#define MAX_PATH_LENGTH 100
#define MAX_DIR_DEPTH 4


int main(int argc, char const* argv[]) {
    char ramfs_path[MAX_PATH_LENGTH] = "/some/ramfs";
    char another_ramfs_path[MAX_PATH_LENGTH] = "/some/ramfs/some/another";

    if (mkdir("/some", 0777) == -1) {
        perror("Failed to create directory under /some");
        return 1;
    }

    // Create a directory under /some/ramfs
    if (mkdir("/some/ramfs", 0777) == -1) {
        perror("Failed to create directory under /some/ramfs");
        return 1;
    }

    // Mount the first ramfs at /some/ramfs
    if (mount("", "/some/ramfs", "ramfs", 0, NULL) == -1) {
        perror("Failed to mount ramfs at /some/ramfs");
        return 1;
    }

    if (mkdir("/some/ramfs/some", 0777) == -1) {
        perror("Failed to create directory under /some/ramfs/some");
        return 1;
    }

    // Create a directory under /some/ramfs/some/another
    if (mkdir("/some/ramfs/some/another", 0777) == -1) {
        perror("Failed to create directory under /some/ramfs/some/another");
        return 1;
    }

    if (mount("", "/some/ramfs/some/another", "ramfs", 0, NULL) == -1) {
        perror("Failed to mount ramfs at /some/ramfs/some/another");
        return 1;
    }
    if (mkdir("/some/ramfs/some/another/just_another", 0777) == -1) {
        perror("Failed to create directory under /some/ramfs/some/another");
        return 1;
    }

    if (mount("", "/some/ramfs/some/another/just_another", "ramfs", 0, NULL) == -1) {
        perror("Failed to mount ramfs at /some/ramfs/some/another");
        return 1;
    }


    // Write files under /some/ramfs and /some/ramfs/some/another
    FILE* file1 = fopen("/some/ramfs/file1.txt", "w");
    if (file1 == NULL) {
        perror("Failed to open /some/ramfs/file1.txt");
        return 1;
    }
    fprintf(file1, "This is file1.txt\n");
    fclose(file1);

    FILE* file2 = fopen("/some/ramfs/some/another/file2.txt", "w");
    if (file2 == NULL) {
        perror("Failed to open /some/ramfs/some/another/file2.txt");
        return 1;
    }
    fprintf(file2, "This is file2.txt\n");
    fclose(file2);

    FILE* file3 = fopen("/some/ramfs/some/another/just_another/file3.txt", "w");
    if (file2 == NULL) {
        perror("Failed to open /some/ramfs/some/another/just_another/file3.txt");
        return 1;
    }
    fprintf(file3, "This is file3.txt\n");
    fclose(file3);

    // // Delete files under /some/ramfs and /some/ramfs/some/another
    // if (unlink("/some/ramfs/file1.txt") == -1) {
    //     perror("Failed to delete /some/ramfs/file1.txt");
    //     return 1;
    // }

    // if (unlink("/some/ramfs/some/another/file2.txt") == -1) {
    //     perror("Failed to delete /some/ramfs/some/another/file2.txt");
    //     return 1;
    // }

    // Remove directories under /some/ramfs and /some/ramfs/some/another
    // if (rmdir(another_ramfs_path) == -1) {
    //     perror("Failed to remove directory under /some/ramfs/some/another");
    //     return 1;
    // }
    // if (rmdir(ramfs_path) == -1) {
    //     perror("Failed to remove directory under /some/ramfs");
    //     return 1;
    // }


    return 0;
}