#include <stdio.h>
#include <unistd.h>
#include <sys/select.h>

int main(void) {
    fd_set rfds;
    int retval;

    /* Watch stdin (fd 0) to see when it has input. */
    FD_ZERO(&rfds);
    FD_SET(0, &rfds);

    /* timeout 设为 NULL 表示无限等待 */
    retval = select(1, &rfds, NULL, NULL, NULL);

    if (retval == -1) {
        perror("select()");
    } else {
        if (FD_ISSET(0, &rfds)) {
            char buf[256];
            ssize_t n = read(0, buf, sizeof(buf) - 1);
            if (n > 0) {
                buf[n] = '\0';
                printf("Read %zd bytes from stdin: %s", n, buf);
            } else if (n == 0) {
                printf("EOF on stdin\n");
            } else {
                perror("read");
            }
        }
    }

    return 0;
}
