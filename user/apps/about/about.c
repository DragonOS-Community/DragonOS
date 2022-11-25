#include <libc/math.h>
#include <libc/stdio.h>
#include <libc/stdlib.h>
#include <libc/time.h>
#include <libc/unistd.h>

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
    put_string("V0.1.0 - 20221106\n", COLOR_GREEN, COLOR_BLACK);
    printf(" You can visit the project via:\n");
    printf("\n");
    put_string("    Official Website: https://DragonOS.org\n", COLOR_INDIGO, COLOR_BLACK);
    put_string("    GitHub: https://github.com/fslongjin/DragonOS\n", COLOR_ORANGE, COLOR_BLACK);
    printf("\n");
    printf(" Maintainer: longjin <longjin@RinGoTek.cn>\n");
    printf(" Get contact with the community: <contact@DragonOS.org>\n");
    printf("\n");
    printf(" If you find any problems during use, please visit:\n");
    put_string("    https://bbs.DragonOS.org\n", COLOR_ORANGE, COLOR_BLACK);
    printf("\n");
}

int main()
{
    // printf("Hello World!\n");
    print_ascii_logo();

    print_copyright();
    // exit(0);
    // while (1)
    //     ;

    return 0;
}