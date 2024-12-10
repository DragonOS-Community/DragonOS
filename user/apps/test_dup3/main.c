#include <stdio.h>
#include <unistd.h>
#include <fcntl.h>

int main() {
    int fd = open("/history_commands.txt", O_RDONLY);
    if (fd < 0) {
        perror("Failed to open file");
        return 1;
    }

    int new_fd = 777;
    int rt = dup3(fd, new_fd, O_CLOEXEC);
    if (rt < 0) {
        perror("Failed to duplicate file descriptor with flags");
    }

    char buffer[100];
    int bytes_read = read(new_fd, buffer, sizeof(buffer));
    if (bytes_read < 0) {
        perror("Failed to read data");
        return 1;
    }

    printf("Data:\n %.*s\n", bytes_read, buffer);

    close(fd);
    close(new_fd);
    return 0;
}