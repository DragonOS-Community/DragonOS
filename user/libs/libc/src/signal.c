#include <libc/src/include/signal.h>
#include <libc/src/printf.h>
#include <libc/src/stddef.h>
#include <libsystem/syscall.h>

void __libc_sa_restorer()
{
    // todo: 在这里发起sigreturn
    while (1)
    {
        /* code */
    }
}

int signal(int signum, __sighandler_t handler)
{
    struct sigaction sa = {0};
    sa.sa_handler = handler;
    sa.sa_restorer = &__libc_sa_restorer;
    printf("handler address: %#018lx\n", handler);
    printf("restorer address: %#018lx\n", &__libc_sa_restorer);
    sigaction(SIGKILL, &sa, NULL);
}

int sigaction(int signum, const struct sigaction *act, struct sigaction *oldact)
{
    return syscall_invoke(SYS_SIGACTION, (uint64_t)signum, (uint64_t)act, (uint64_t)oldact, 0, 0, 0, 0, 0);
}