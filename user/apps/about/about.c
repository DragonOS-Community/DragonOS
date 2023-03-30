#include "sys_version.h"    // 这是系统的版本头文件，在编译过程中自动生成
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
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
    printf(" Copyright: fslongjin & DragonOS Community. 2022, All rights reserved.\n");
    printf(" Version: ");
    put_string("V0.1.5\n", COLOR_GREEN, COLOR_BLACK);
    printf(" Git commit SHA1: %s\n", DRAGONOS_GIT_COMMIT_SHA1);
    printf(" Build time: %s %s\n", __DATE__, __TIME__);
    printf(" \nYou can visit the project via:\n");
    printf("\n");
    put_string("    Official Website: https://DragonOS.org\n", COLOR_INDIGO, COLOR_BLACK);
    put_string("    GitHub: https://github.com/DragonOS-Community/DragonOS\n", COLOR_ORANGE, COLOR_BLACK);
    printf("\n");
    printf(" Maintainer: longjin <longjin@DragonOS.org>\n");
    printf(" Get contact with the community: <contact@DragonOS.org>\n");
    printf("\n");
    printf(" If you find any problems during use, please visit:\n");
    put_string("    https://bbs.DragonOS.org\n", COLOR_ORANGE, COLOR_BLACK);
    printf("\n");
}

int main()
{   
    print_ascii_logo();
    print_copyright();

    return 0;
}