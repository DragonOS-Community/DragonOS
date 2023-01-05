
#include <stdio.h>
#include <stdlib.h>

extern int main(int, char **);
extern void _init();

void _start(int argc, char **argv)
{
    // Run the global constructors.
    _init();
    int retval = main(argc, argv);
    // printf("before exit, code=%d\n", retval);
    exit(retval);
}