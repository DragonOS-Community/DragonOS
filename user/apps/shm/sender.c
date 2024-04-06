#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <sys/mman.h>
#include <sys/shm.h>
#include <sys/ipc.h>
#include <string.h>
#include <sys/wait.h>

#define SHM_SIZE 9999

int main()
{
    int shmid;
    char *shmaddr;
    key_t key = 6666;

    // 测试shmget
    shmid = shmget(key, SHM_SIZE, 0666 | IPC_CREAT);
    if (shmid < 0)
    {
        perror("shmget failed");
        exit(EXIT_FAILURE);
    }

    // 测试shmat
    shmaddr = shmat(shmid, 0, 0);

    memset(shmaddr, 0, SHM_SIZE);
    memcpy(shmaddr, "Sender Hello!", 14);

    int pid = fork();
    if (pid == 0)
    {
        execl("/bin/receiver", NULL, NULL);
    }

    waitpid(pid, NULL, 0);

    char read_buf[20];
    memcpy(read_buf, shmaddr, 16);
    printf("Sender receive: %s\n", read_buf);

    shmdt(shmaddr);
    shmctl(shmid, IPC_RMID, NULL);
}
