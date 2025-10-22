#include <stdio.h>
#include <unistd.h>
#include <sys/reboot.h>

int main(int argc, char const* argv[]) {
    printf("Test reboot (restart mode)\n");

    if (reboot(RB_AUTOBOOT) == -1) {
        perror("Failed to reboot (restart)");
        return 1;
    }

    return 0;
}