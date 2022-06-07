#include <libc/stdio.h>

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
    printf(" Copyright: fslongjin. 2022, All rights reserved.\n");
    printf(" You can visit the project via:\n");
    printf("\n");
    put_string("    https://github.com/fslongjin/DragonOS\n", COLOR_ORANGE, COLOR_BLACK);
    printf("\n");
    printf("    Email: longjin@RinGoTek.cn\n");
    printf("\n");
}
int main()
{
    // printf("Hello World!\n");
    print_ascii_logo();
    print_copyright();
    exit(1);
    while (1)
        ;
}