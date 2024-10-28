#include <stdio.h>
#include <stdlib.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <unistd.h>
#include <fcntl.h>
#include <string.h>
#include <errno.h>

#define LOWERDIR "/tmp/overlayfs/lower"
#define UPPERDIR "/tmp/overlayfs/upper"
#define WORKDIR "/tmp/overlayfs/work"
#define MERGEDDIR "/tmp/overlayfs/merged"

void create_directories()
{
    mkdir(LOWERDIR, 0755);
    mkdir(UPPERDIR, 0755);
    mkdir(WORKDIR, 0755);
    mkdir(MERGEDDIR, 0755);
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
}

void read_merged_file()
{
    char filepath[256];
    snprintf(filepath, sizeof(filepath), "%s/lowerfile.txt", MERGEDDIR);

    char buffer[256];
    int fd = open(filepath, O_RDONLY);
    if (fd < 0)
    {
        perror("Failed to open file in merged dir");
        exit(EXIT_FAILURE);
    }
    read(fd, buffer, sizeof(buffer));
    printf("Read from merged file: %s", buffer);
    close(fd);
}

void create_upper_file()
{
    char filepath[256];
    snprintf(filepath, sizeof(filepath), "%s/upperfile.txt", MERGEDDIR);

    int fd = open(filepath, O_CREAT | O_WRONLY, 0644);
    if (fd < 0)
    {
        perror("Failed to create file in upperdir");
        exit(EXIT_FAILURE);
    }
    write(fd, "This is an upper layer file.\n", 29);
    close(fd);
    printf("File created in upper layer.\n");
}

void umount_overlayfs()
{
    if (umount(MERGEDDIR) != 0)
    {
        perror("Unmount failed");
        exit(EXIT_FAILURE);
    }
    printf("OverlayFS unmounted successfully.\n");
}

int main()
{
    create_directories();
    create_lower_file();
    mount_overlayfs();
    read_merged_file();
    create_upper_file();
    umount_overlayfs();
    return 0;
}
