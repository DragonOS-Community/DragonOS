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

    if (mkdir("/SOME", 0777) == -1) {
        perror("Failed to create directory under /some");
        return 1;
    }

    // Create a directory under /SOME/RAMFS
    if (mkdir("/SOME/RAMFS", 0777) == -1) {
        perror("Failed to create directory under /SOME/RAMFS");
        return 1;
    }

    // Mount the first ramfs at /SOME/RAMFS
    if (mount("", "/SOME/RAMFS", "ramfs", 0, NULL) == -1) {
        perror("Failed to mount ramfs at /SOME/RAMFS");
        return 1;
    }

    if (mkdir("/SOME/RAMFS/some", 0777) == -1) {
        perror("Failed to create directory under /SOME/RAMFS/some");
        return 1;
    }

    puts("Success mkdir /SOME/RAMFS/some");

    // Create a directory under /SOME/RAMFS/some/another
    if (mkdir("/SOME/RAMFS/some/another", 0777) == -1) {
        perror("Failed to create directory under /SOME/RAMFS/some/another");
        return 1;
    }

    puts("Success mkdir /SOME/RAMFS/some/another");

    if (mount("", "/SOME/RAMFS/some/another", "ramfs", 0, NULL) == -1) {
        perror("Failed to mount ramfs at /SOME/RAMFS/some/another");
        return 1;
    }

    puts("Success mount on /SOME/RAMFS/some/another");

    if (mkdir("/SOME/RAMFS/some/another/just_another", 0777) == -1) {
        perror("Failed to create directory under /SOME/RAMFS/some/another");
        return 1;
    }

    puts("Success mkdir /SOME/RAMFS/some/another/just_another");

    if (mount("", "/SOME/RAMFS/some/another/just_another", "ramfs", 0, NULL) == -1) {
        perror("Failed to mount ramfs at /SOME/RAMFS/some/another");
        return 1;
    }

    puts("Success mount on /SOME/RAMFS/some/another/just_another");

    // Write files under /SOME/RAMFS and /SOME/RAMFS/some/another
    FILE* file1 = fopen("/SOME/RAMFS/file1.txt", "w");
    if (file1 == NULL) {
        perror("Failed to open /SOME/RAMFS/file1.txt");
        return 1;
    }
    fprintf(file1, "This is file1.txt\n");
    fclose(file1);

    FILE* file2 = fopen("/SOME/RAMFS/some/another/file2.txt", "w");
    if (file2 == NULL) {
        perror("Failed to open /SOME/RAMFS/some/another/file2.txt");
        return 1;
    }
    fprintf(file2, "This is file2.txt\n");
    fclose(file2);

    FILE* file3 = fopen("/SOME/RAMFS/some/another/just_another/file3.txt", "w+");
    if (file3 == NULL) {
        perror("Failed to open /SOME/RAMFS/some/another/just_another/file3.txt");
        return 1;
    }
    fprintf(file3, "Multi mount behave well.\n");
    // print file3.txt
    char buffer[100];
    fseek(file3, 0, SEEK_SET);
    fread(buffer, 1, 100, file3);
    printf("file3.txt content: %s\n", buffer);
    fclose(file3);

    // test umount with flags ( use umount2 )
    if (umount("/SOME/RAMFS/some/another/just_another") == -1) {
        perror("Failed to umount ramfs at /SOME/RAMFS/some/another/just_another");
        return 1;
    }

    puts("Successful umount /SOME/RAMFS/some/another/just_another");

    // delete just_another
    if (rmdir("/SOME/RAMFS/some/another/just_another") == -1) {
        perror("Failed to delete /SOME/RAMFS/some/another/just_another");
        return 1;
    }

    return 0;
}