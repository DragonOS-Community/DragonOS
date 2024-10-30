#include <stdio.h>
#include <stdlib.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <unistd.h>
#include <fcntl.h>
#include <string.h>
#include <errno.h>

// #define LOWERDIR "/tmp/overlayfs/lower"
// #define UPPERDIR "/tmp/overlayfs/upper"
// #define WORKDIR "/tmp/overlayfs/work"
// #define MERGEDDIR "/tmp/overlayfs/merged"

// void create_directories()
// {
//     mkdir(LOWERDIR, 0755);
//     mkdir(UPPERDIR, 0755);
//     mkdir(WORKDIR, 0755);
//     mkdir(MERGEDDIR, 0755);
// }
#define TMPDIR "/tmp"
#define OVERLAYFSDIR "/tmp/overlayfs"
#define LOWERDIR "/tmp/overlayfs/lower"
#define UPPERDIR "/tmp/overlayfs/upper"
#define WORKDIR "/tmp/overlayfs/work"
#define MERGEDDIR "/tmp/overlayfs/merged"

void create_directories()
{
    mkdir(TMPDIR, 0755);
    mkdir(OVERLAYFSDIR, 0755);
    mkdir(LOWERDIR, 0755);
    mkdir(UPPERDIR, 0755);
    mkdir(WORKDIR, 0755);
    mkdir(MERGEDDIR, 0755);
    printf("step1 : success\n");
}

void create_lower_file()
{
    char filepath[256];
    snprintf(filepath, sizeof(filepath), "%s/lowerfile.txt", LOWERDIR);

    int fd = open(filepath, O_CREAT | O_WRONLY, 0644);
    if (fd < 0)
    {
        perror("Failed to create file in lowerdir");
        exit(EXIT_FAILURE);
    }
    write(fd, "This is a lower layer file.\n", 28);
    close(fd);
    printf("step2 : success\n");
}

void mount_overlayfs()
{
    char options[1024];
    snprintf(options, sizeof(options),
             "lowerdir=%s,upperdir=%s,workdir=%s",
             LOWERDIR, UPPERDIR, WORKDIR);

    if (mount("overlay", MERGEDDIR, "overlay", 0, options) != 0)
    {
        perror("Mount failed");
        exit(EXIT_FAILURE);
    }
    printf("OverlayFS mounted successfully.\n");
    printf("step3 : success\n");
}

void create_directory_in_merged()
{
    char dirpath[256];
    snprintf(dirpath, sizeof(dirpath), "%s/newdir", UPPERDIR);

    if (mkdir(dirpath, 0755) != 0)
    {
        perror("Failed to create directory in merged dir");
        exit(EXIT_FAILURE);
    }
    printf("Directory created in merged: %s\n", dirpath);
    printf("step4 : success\n");
}

int main()
{
    create_directories();
    mount_overlayfs();
    create_directory_in_merged();
    return 0;
}