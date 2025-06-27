#include <stdio.h>
#include <unistd.h>
#include <sys/reboot.h>

int main(int argc, char const* argv[]) {
    printf("Test reboot and halt\n");

    if (reboot(RB_HALT_SYSTEM) == -1) {
        perror("Failed to halt the system");
        return 1;
    }

    return 0;
}