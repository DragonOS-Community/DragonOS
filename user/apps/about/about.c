#include "sys_version.h" // 这是系统的版本头文件，在编译过程中自动生成
#include <stdio.h>
#include <unistd.h>

void print_ascii_logo()
{
    printf(" ____                                      ___   ____ \n");
    printf("|  _ \\  _ __   __ _   __ _   ___   _ __   / _ \\ / ___| \n");
    printf("| | | || '__| / _` | / _` | / _ \\ | '_ \\ | | | |\\___ \\  \n");
    printf("| |_| || |   | (_| || (_| || (_) || | | || |_| | ___) |\n");
    printf("|____/ |_|    \\__,_| \\__, | \\___/ |_| |_| \\___/ |____/ \n");
    printf("                     |___/     \n");
}

void print_copyright()
{
    printf(" DragonOS - An opensource operating system.\n");
    printf(" Copyright: DragonOS Community. 2022-2024, All rights reserved.\n");
    printf(" Version: ");
    printf("\033[1;32m%s\033[0m", "V0.1.8\n");
    printf(" Git commit SHA1: %s\n", DRAGONOS_GIT_COMMIT_SHA1);
    printf(" Build time: %s %s\n", __DATE__, __TIME__);
    printf(" \nYou can visit the project via:\n");
    printf("\n");
    printf("\x1B[1;36m%s\x1B[0m", "    Official Website: https://DragonOS.org\n");
    printf("\x1B[1;33m%s\x1B[0m", "    GitHub: https://github.com/DragonOS-Community/DragonOS\n");
    printf("\n");
    printf(" Maintainer: longjin <longjin@DragonOS.org>\n");
    printf(" Get contact with the community: <contact@DragonOS.org>\n");
    printf("\n");
    printf(" Join our development community:\n");
    printf("\x1B[1;33m%s\x1B[0m", "    https://bbs.dragonos.org.cn\n");
    printf("\n");
}

int main()
{
    print_ascii_logo();
    print_copyright();
    return 0;
}
