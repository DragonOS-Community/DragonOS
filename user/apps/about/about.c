#include "sys_version.h" // 这是系统的版本头文件，在编译过程中自动生成
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>
#include <fcntl.h>
// void print_ascii_logo()
// {
//     printf(" ____                                      ___   ____ \n");
//     printf("|  _ \\  _ __   __ _   __ _   ___   _ __   / _ \\ / ___| \n");
//     printf("| | | || '__| / _` | / _` | / _ \\ | '_ \\ | | | |\\___ \\  \n");
//     printf("| |_| || |   | (_| || (_| || (_) || | | || |_| | ___) |\n");
//     printf("|____/ |_|    \\__,_| \\__, | \\___/ |_| |_| \\___/ |____/ \n");
//     printf("                     |___/     \n");
// }
// void print_copyright()
// {
//     printf(" DragonOS - An opensource operating system.\n");
//     printf(" Copyright: fslongjin & DragonOS Community. 2022-2023, All rights reserved.\n");
//     printf(" Version: ");
//     put_string("V0.1.8\n", COLOR_GREEN, COLOR_BLACK);
//     printf(" Git commit SHA1: %s\n", DRAGONOS_GIT_COMMIT_SHA1);
//     printf(" Build time: %s %s\n", __DATE__, __TIME__);
//     printf(" \nYou can visit the project via:\n");
//     printf("\n");
//     put_string("    Official Website: https://DragonOS.org\n", COLOR_INDIGO, COLOR_BLACK);
//     put_string("    GitHub: https://github.com/DragonOS-Community/DragonOS\n", COLOR_ORANGE, COLOR_BLACK);
//     printf("\n");
//     printf(" Maintainer: longjin <longjin@DragonOS.org>\n");
//     printf(" Get contact with the community: <contact@DragonOS.org>\n");
//     printf("\n");
//     printf(" If you find any problems during use, please visit:\n");
//     put_string("    https://bbs.DragonOS.org\n", COLOR_ORANGE, COLOR_BLACK);
//     printf("\n");
//     printf(" Join our development community:\n");
//     put_string("    https://DragonOS.zulipchat.com\n", COLOR_ORANGE, COLOR_BLACK);
//     printf("\n");
// }

int main()
{
    // print_ascii_logo();
    // print_copyright();
        int pipe_fds[2];
    int result = pipe2(pipe_fds, O_NONBLOCK);
    if (result == -1) {
        printf("pipe2 failed");
        return;
    }

    int read_fd = pipe_fds[0];
    int write_fd = pipe_fds[1];

    printf("Pipe created with read_fd=%d and write_fd=%d\n", read_fd, write_fd);

    // Write data to the pipe
    const char* message = "Hello, pipe!";
    ssize_t bytes_written = write(write_fd, message, strlen(message));
    if (bytes_written == -1) {
        printf("Write failed");
        return;
    }

    printf("Data written to the pipe: %s\n", message);

    // Read data from the pipe
    char buffer[1024];
    ssize_t bytes_read = read(read_fd, buffer, sizeof(buffer));
    if (bytes_read == -1) {
        printf("Read failed");
        return;
    }

    buffer[bytes_read] = '\0';
    printf("Data read from the pipe: %s\n", buffer);

    close(read_fd);
    close(write_fd);

    return 0;
}