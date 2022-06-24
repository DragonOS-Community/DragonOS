
extern int main(int, char**);
#include<libc/stdio.h>
void _start(int argc, char** argv)
{
    printf("before main\n");
    int retval = main(argc, argv);
    printf("before exit, code=%d\n", retval);
    exit(retval);
}